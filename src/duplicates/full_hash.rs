use super::{digest, Member, Progress, MIB};
use std::{
    io,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed},
        mpsc,
    },
    thread,
    time::Duration,
};

pub(super) fn calculate(
    jobs: &[&Member],
    workers: usize,
    cancelled: &impl Fn() -> bool,
    progress: &mut Progress,
    notify: &mut impl FnMut(Progress),
) -> io::Result<Vec<io::Result<[u8; 32]>>> {
    crate::output::check(cancelled)?;
    let stop = AtomicBool::new(false);
    let bytes = AtomicU64::new(0);
    let next = AtomicUsize::new(0);
    let (stop, bytes, next) = (&stop, &bytes, &next);
    let mut results: Vec<_> = (0..jobs.len()).map(|_| None).collect();

    // 작업당 파일 하나와 1 MiB 버퍼만 유지하고 종료 전에 모든 작업 스레드 정리.
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::sync_channel(workers);
        for _ in 0..workers {
            let sender = sender.clone();
            let started = thread::Builder::new().spawn_scoped(scope, move || {
                let mut buffer = vec![0; MIB as usize];
                while !stop.load(Relaxed) {
                    let index = next.fetch_add(1, Relaxed);
                    let Some(member) = jobs.get(index) else {
                        break;
                    };
                    let mut consumed = 0;
                    let result = digest(
                        member,
                        false,
                        &mut buffer,
                        &|| stop.load(Relaxed),
                        &mut Progress::default(),
                        &mut |p| {
                            let delta = p.bytes - consumed;
                            consumed = p.bytes;
                            bytes.fetch_add(delta, Relaxed);
                        },
                    );
                    if sender.send((index, result)).is_err() {
                        break;
                    }
                }
            });
            if let Err(error) = started {
                stop.store(true, Relaxed);
                return Err(error);
            }
        }
        drop(sender);

        let mut remaining = jobs.len();
        while remaining > 0 {
            if let Err(error) = crate::output::check(cancelled) {
                stop.store(true, Relaxed);
                return Err(error);
            }
            match receiver.recv_timeout(Duration::from_millis(4)) {
                Ok((_, Err(error))) if error.kind() == io::ErrorKind::Interrupted => {
                    stop.store(true, Relaxed);
                    return Err(error);
                }
                Ok((index, result)) => {
                    results[index] = Some(result);
                    remaining -= 1;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other(
                        "전체 내용 검사 작업이 예기치 않게 종료됐습니다",
                    ));
                }
            }
            let consumed = bytes.swap(0, Relaxed);
            if consumed != 0 {
                progress.bytes += consumed;
                notify(*progress);
            }
        }
        crate::output::check(cancelled)
    })?;

    Ok(results
        .into_iter()
        .map(|result| result.expect("각 작업의 응답 수신 완료"))
        .collect())
}
