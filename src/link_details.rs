use std::{io, path::PathBuf, sync::mpsc};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub path: PathBuf,
    pub identity: crate::metadata::Identity,
    pub source: u64,
    pub generation: u64,
}
#[derive(Default)]
pub struct Controller {
    key: Option<Key>,
    receiver: Option<mpsc::Receiver<io::Result<u32>>>,
    pub value: Option<Result<u32, String>>,
}
pub fn read(path: &std::path::Path, identity: crate::metadata::Identity) -> io::Result<u32> {
    let report = crate::metadata::read(path);
    if let Some(error) = report.issue {
        return Err(error);
    }
    if report.values.identity.value() != Some(identity) {
        return Err(io::Error::other(
            "파일이 변경됐습니다. 다시 스캔한 뒤 확인하세요",
        ));
    }
    report
        .values
        .links
        .value()
        .ok_or_else(|| io::Error::other("연결 개수를 확인할 수 없습니다"))
}
impl Controller {
    pub fn select(&mut self, key: Option<Key>) {
        if self.key != key {
            self.key = key;
            self.receiver = None;
            self.value = None;
        }
    }
    pub fn busy(&self) -> bool {
        self.receiver.is_some()
    }
    pub fn start(&mut self) -> io::Result<()> {
        let key = self
            .key
            .clone()
            .ok_or_else(|| io::Error::other("확인할 파일이 없습니다"))?;
        self.start_with(move || read(&key.path, key.identity))
    }
    pub fn start_with(
        &mut self,
        operation: impl FnOnce() -> io::Result<u32> + Send + 'static,
    ) -> io::Result<()> {
        if self.busy() {
            return Err(io::Error::other("연결 개수를 확인 중입니다"));
        }
        let (send, wait) = mpsc::channel();
        self.receiver = Some(wait);
        self.value = None;
        std::thread::spawn(move || {
            let _ = send.send(operation());
        });
        Ok(())
    }
    pub fn poll(&mut self) {
        let Some(wait) = &self.receiver else {
            return;
        };
        let result = match wait.try_recv() {
            Ok(v) => v,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(io::Error::other("연결 개수 조회가 중단됐습니다"))
            }
        };
        self.receiver = None;
        self.value = Some(result.map_err(|e| e.to_string()));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn demand_query_checks_identity_and_ignores_an_old_selection() {
        let root = std::env::temp_dir().join(format!("diskweave-links-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("file");
        std::fs::write(&path, b"123").unwrap();
        std::fs::hard_link(&path, root.join("alias")).unwrap();
        let id = crate::metadata::read(&path)
            .values
            .identity
            .value()
            .unwrap();
        assert_eq!(read(&path, id).unwrap(), 2);
        let mut wrong = id;
        wrong.file[0] ^= 1;
        assert!(read(&path, wrong).is_err());
        let mut job = Controller::default();
        let key = Key {
            path: path.clone(),
            identity: id,
            source: 1,
            generation: 1,
        };
        job.select(Some(key.clone()));
        let (send, wait) = mpsc::channel();
        job.start_with(move || {
            wait.recv().unwrap();
            Ok(2)
        })
        .unwrap();
        job.select(None);
        send.send(()).unwrap();
        job.poll();
        assert!(job.value.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
