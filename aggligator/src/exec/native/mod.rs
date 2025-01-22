//! Native async executive using Tokio.

pub mod runtime {
    pub use tokio::runtime::Handle;
}

pub mod task {
    pub use tokio::task::{spawn, JoinError, JoinHandle};
}

pub mod time {
    pub use tokio::time::{sleep, sleep_until, timeout, Instant, Sleep, Timeout};
    pub use tokio_stream::wrappers::IntervalStream;

    pub fn interval_stream(period: std::time::Duration) -> IntervalStream {
        IntervalStream::new(tokio::time::interval(period))
    }

    pub mod error {
        pub use tokio::time::error::Elapsed;
    }
}
