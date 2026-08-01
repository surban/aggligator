//! Native async executive using Tokio.

pub mod runtime {
    pub use tokio::runtime::Handle;
}

pub mod task {
    pub use tokio::task::{JoinError, JoinHandle, spawn};
}

pub mod time {
    pub use tokio::time::{Instant, Sleep, Timeout, sleep, sleep_until, timeout};
    pub use tokio_stream::wrappers::IntervalStream;

    pub fn interval_stream(period: std::time::Duration) -> IntervalStream {
        IntervalStream::new(tokio::time::interval(period))
    }

    pub mod error {
        pub use tokio::time::error::Elapsed;
    }
}
