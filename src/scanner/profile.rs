use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Instant,
};

#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Stage {
    Overall,
    RootSetup,
    PathPrepare,
    WindowsQuery,
    FolderCheck,
    Enumerate,
    TaskBuild,
    Dispatch,
    ResultSend,
    ResultReceive,
    Coordinator,
    WorkerIdle,
    Finish,
    MetadataTask,
    MftRead,
    MftMerge,
    MftCompact,
    MftNormalize,
    MftGraph,
}
const N: usize = 19;
#[allow(dead_code, reason = "개발 도구의 보고서에서만 사용하는 단계 이름")]
const NAMES: [&str; N] = [
    "overall",
    "root_setup",
    "path_prepare",
    "windows_query",
    "folder_check",
    "enumeration",
    "task_build",
    "dispatch",
    "result_send",
    "result_receive",
    "coordinator",
    "worker_idle",
    "finish",
    "metadata_task",
    "mft_read",
    "mft_merge",
    "mft_compact",
    "mft_normalize",
    "mft_graph",
];
static ALLOC_ENABLED: AtomicBool = AtomicBool::new(false);
thread_local! {
    static ACTIVE:Cell<usize>=const{Cell::new(usize::MAX)};
    static ALLOCS:[Cell<u64>;N*2]=const{[const{Cell::new(0)};N*2]};
}
#[derive(Default)]
struct Bucket {
    calls: AtomicU64,
    ns: AtomicU64,
    allocations: AtomicU64,
    bytes: AtomicU64,
}
pub struct Trace {
    buckets: [Bucket; N],
    cpu: [CpuBucket; 2],
    allocation_supported: bool,
}
impl Default for Trace {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| Bucket::default()),
            cpu: std::array::from_fn(|_| CpuBucket::default()),
            allocation_supported: ALLOC_ENABLED.load(Ordering::Relaxed),
        }
    }
}
#[derive(serde::Serialize)]
#[allow(dead_code, reason = "개발 도구에서만 생성하는 보고서")]
pub struct StageReport {
    pub stage: &'static str,
    pub calls: u64,
    pub elapsed_ns: u64,
    pub allocations: Option<u64>,
    pub allocated_bytes: Option<u64>,
}
#[derive(serde::Serialize)]
#[allow(dead_code, reason = "개발 도구에서만 생성하는 보고서")]
pub struct CpuReport {
    pub measured_samples: u64,
    pub missing_samples: u64,
    pub user_ns: Option<u64>,
    pub kernel_ns: Option<u64>,
}
#[derive(serde::Serialize)]
#[allow(dead_code, reason = "개발 도구에서만 생성하는 보고서")]
pub struct Report {
    pub stages: Vec<StageReport>,
    pub worker_cpu: Option<CpuReport>,
    pub coordinator_cpu: Option<CpuReport>,
}
impl Trace {
    #[allow(dead_code, reason = "개발 도구에서만 요청하는 보고서")]
    pub fn snapshot(&self) -> Report {
        let load = |v: &AtomicU64| v.load(Ordering::Relaxed);
        let cpu = |b: &CpuBucket| {
            let samples = load(&b.samples);
            let missing = load(&b.missing);
            (samples + missing > 0).then(|| CpuReport {
                measured_samples: samples,
                missing_samples: missing,
                user_ns: (missing == 0).then(|| load(&b.user)),
                kernel_ns: (missing == 0).then(|| load(&b.kernel)),
            })
        };
        Report {
            stages: self
                .buckets
                .iter()
                .enumerate()
                .map(|(i, b)| StageReport {
                    stage: NAMES[i],
                    calls: load(&b.calls),
                    elapsed_ns: load(&b.ns),
                    allocations: self.allocation_supported.then(|| load(&b.allocations)),
                    allocated_bytes: self.allocation_supported.then(|| load(&b.bytes)),
                })
                .collect(),
            worker_cpu: cpu(&self.cpu[0]),
            coordinator_cpu: cpu(&self.cpu[1]),
        }
    }
}
pub struct Guard<'a> {
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
    trace: &'a Trace,
    stage: usize,
    start: Instant,
    previous: usize,
    alloc_start: (u64, u64),
}
/// 계측이 켜진 단계만 측정. 시간은 중첩 구간을 포함하고 할당은 현재 단계에만 귀속.
pub fn scope(trace: Option<&Trace>, stage: Stage) -> Option<Guard<'_>> {
    trace.map(|trace| {
        let stage = stage as usize;
        let previous = if trace.allocation_supported {
            ACTIVE.with(|a| a.replace(stage))
        } else {
            usize::MAX
        };
        let alloc_start = ALLOCS.with(|a| (a[stage * 2].get(), a[stage * 2 + 1].get()));
        Guard {
            _thread: std::marker::PhantomData,
            trace,
            stage,
            start: Instant::now(),
            previous,
            alloc_start,
        }
    })
}
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let b = &self.trace.buckets[self.stage];
        b.calls.fetch_add(1, Ordering::Relaxed);
        b.ns.fetch_add(self.start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if self.trace.allocation_supported {
            ALLOCS.with(|a| {
                b.allocations.fetch_add(
                    a[self.stage * 2].get() - self.alloc_start.0,
                    Ordering::Relaxed,
                );
                b.bytes.fetch_add(
                    a[self.stage * 2 + 1].get() - self.alloc_start.1,
                    Ordering::Relaxed,
                );
                a[self.stage * 2].set(self.alloc_start.0);
                a[self.stage * 2 + 1].set(self.alloc_start.1);
            });
            ACTIVE.with(|a| a.set(self.previous));
        }
    }
}

#[derive(Default)]
struct CpuBucket {
    samples: AtomicU64,
    missing: AtomicU64,
    user: AtomicU64,
    kernel: AtomicU64,
}
#[derive(Clone, Copy)]
pub struct CpuTime {
    pub user_ns: u64,
    pub kernel_ns: u64,
}
pub fn thread_cpu() -> Option<CpuTime> {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetCurrentThread, GetThreadTimes},
    };
    let mut created = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // 현재 스레드의 닫지 않는 의사 핸들만 사용. 성공한 출력의 100ns 단위를 ns로 변환.
    if unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut created,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return None;
    }
    let ns =
        |v: FILETIME| (((v.dwHighDateTime as u64) << 32) | v.dwLowDateTime as u64).checked_mul(100);
    Some(CpuTime {
        user_ns: ns(user)?,
        kernel_ns: ns(kernel)?,
    })
}
pub struct CpuGuard<'a> {
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
    trace: &'a Trace,
    role: usize,
    start: Option<CpuTime>,
    clock: fn() -> Option<CpuTime>,
}
pub fn cpu_scope(
    trace: Option<&Trace>,
    worker: bool,
    clock: fn() -> Option<CpuTime>,
) -> Option<CpuGuard<'_>> {
    trace.map(|trace| CpuGuard {
        _thread: std::marker::PhantomData,
        trace,
        role: usize::from(!worker),
        start: clock(),
        clock,
    })
}
impl Drop for CpuGuard<'_> {
    fn drop(&mut self) {
        let b = &self.trace.cpu[self.role];
        let delta = self.start.zip((self.clock)()).and_then(|(s, e)| {
            Some((
                e.user_ns.checked_sub(s.user_ns)?,
                e.kernel_ns.checked_sub(s.kernel_ns)?,
            ))
        });
        if let Some((user, kernel)) = delta {
            b.samples.fetch_add(1, Ordering::Relaxed);
            b.user.fetch_add(user, Ordering::Relaxed);
            b.kernel.fetch_add(kernel, Ordering::Relaxed);
        } else {
            b.missing.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// 개발용 실행 파일에서만 등록하는 Rust 할당 계측. Windows 내부 할당·해제 후 잔존량은 측정 대상 아님.
#[allow(dead_code, reason = "제품 기본 allocator를 바꾸지 않는 개발용 계측")]
pub struct Allocator;
#[allow(dead_code, reason = "개발용 실행 파일에서만 할당 계측 활성화")]
pub fn enable_allocations() {
    ALLOC_ENABLED.store(true, Ordering::Relaxed);
}
#[allow(dead_code, reason = "개발용 allocator에서만 호출")]
fn allocation(size: usize) {
    if !ALLOC_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let _ = ACTIVE.try_with(|active| {
        let stage = active.get();
        if stage < N {
            let _ = ALLOCS.try_with(|a| {
                a[stage * 2].set(a[stage * 2].get() + 1);
                a[stage * 2 + 1].set(a[stage * 2 + 1].get() + size as u64);
            });
        }
    });
}
// System의 정렬/수명 계약을 그대로 전달하고 성공한 할당 요청만 기록.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let value = unsafe { System.alloc(layout) };
        if !value.is_null() {
            allocation(layout.size());
        }
        value
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let value = unsafe { System.alloc_zeroed(layout) };
        if !value.is_null() {
            allocation(layout.size());
        }
        value
    }
    unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let value = unsafe { System.realloc(p, layout, size) };
        if !value.is_null() {
            allocation(size);
        }
        value
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unavailable_cpu_is_not_reported_as_zero_cpu() {
        let trace = Trace::default();
        {
            let _cpu = cpu_scope(Some(&trace), true, || None);
        }
        let cpu = trace.snapshot().worker_cpu.unwrap();
        assert_eq!(cpu.measured_samples, 0);
        assert_eq!(cpu.missing_samples, 1);
        assert!(cpu.user_ns.is_none());
        assert!(cpu.kernel_ns.is_none());
    }
    #[test]
    fn public_profile_distinguishes_unmeasured_cpu_and_keeps_phase_units() {
        let trace = Trace::default();
        assert!(trace.snapshot().worker_cpu.is_none());
        {
            let _whole = scope(Some(&trace), Stage::Overall);
            let _phase = scope(Some(&trace), Stage::PathPrepare);
            std::hint::black_box(vec![1u8; 123]);
        }
        let report = trace.snapshot();
        assert_eq!(report.stages[Stage::Overall as usize].calls, 1);
        assert_eq!(report.stages[Stage::PathPrepare as usize].calls, 1);
        assert!(
            report.stages[Stage::Overall as usize].elapsed_ns
                >= report.stages[Stage::PathPrepare as usize].elapsed_ns
        );
        let untouched = Trace::default();
        {
            let _guard = scope(None, Stage::PathPrepare);
        }
        assert_eq!(
            untouched.snapshot().stages[Stage::PathPrepare as usize].calls,
            0
        );
        thread_local! {static CPU_STEP:Cell<u64>=const{Cell::new(0)};}
        let clock = || {
            CPU_STEP.with(|step| {
                let n = step.get();
                step.set(n + 1);
                Some(CpuTime {
                    user_ns: n * 200,
                    kernel_ns: n * 300,
                })
            })
        };
        for worker in [true, true, false] {
            let _cpu = cpu_scope(Some(&trace), worker, clock);
        }
        let report = trace.snapshot();
        let worker = report.worker_cpu.unwrap();
        assert_eq!(
            (worker.measured_samples, worker.user_ns, worker.kernel_ns),
            (2, Some(400), Some(600))
        );
        let coordinator = report.coordinator_cpu.unwrap();
        assert_eq!(
            (
                coordinator.measured_samples,
                coordinator.user_ns,
                coordinator.kernel_ns
            ),
            (1, Some(200), Some(300))
        );
    }
}
