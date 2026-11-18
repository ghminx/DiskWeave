use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    time::Instant,
};

#[derive(Clone, Copy)]
pub(super) enum Stage {
    ReverseLookup,
    SubtreeRemoval,
    SendBudget,
    ReceiveValidation,
    SnapshotValidation,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static TIMES: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

#[allow(dead_code)]
pub fn start() {
    for time in &TIMES {
        time.store(0, Relaxed);
    }
    ACTIVE.store(true, Relaxed);
}

#[allow(dead_code)]
pub fn finish() -> [u64; 5] {
    ACTIVE.store(false, Relaxed);
    TIMES.each_ref().map(|time| time.load(Relaxed))
}

pub(super) struct Sample(Option<(Stage, Instant)>);
pub(super) fn measure(stage: Stage) -> Sample {
    Sample(ACTIVE.load(Relaxed).then(|| (stage, Instant::now())))
}
impl Drop for Sample {
    fn drop(&mut self) {
        if let Some((stage, start)) = self.0 {
            TIMES[stage as usize].fetch_add(start.elapsed().as_nanos() as u64, Relaxed);
        }
    }
}
