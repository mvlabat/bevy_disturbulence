use bevy::tasks::IoTaskPool;
use std::{future::Future, pin::Pin, time::Duration};

use turbulence::{buffer::BufferPool, runtime::Runtime};

#[derive(Clone, Debug)]
pub struct SimpleBufferPool(pub usize);

impl BufferPool for SimpleBufferPool {
    type Buffer = Box<[u8]>;

    fn acquire(&self) -> Self::Buffer {
        vec![0; self.0].into_boxed_slice()
    }
}

#[derive(Default, Clone)]
pub struct TaskPoolRuntime;

impl TaskPoolRuntime {
    pub fn new() -> Self {
        Self
    }
}

impl Runtime for TaskPoolRuntime {
    type Instant = instant::Instant;
    type Sleep = Pin<Box<dyn Future<Output = ()> + Send>>;

    fn spawn<F: Future<Output = ()> + Send + 'static>(&self, f: F) {
        IoTaskPool::get().spawn(f).detach();
    }

    fn now(&self) -> Self::Instant {
        Self::Instant::now()
    }

    fn elapsed(&self, instant: Self::Instant) -> Duration {
        instant.elapsed()
    }

    fn duration_between(&self, earlier: Self::Instant, later: Self::Instant) -> Duration {
        later.duration_since(earlier)
    }

    fn sleep(&self, duration: Duration) -> Self::Sleep {
        Box::pin(async move {
            wasm_timer::Delay::new(duration).await.unwrap();
        })
    }
}
