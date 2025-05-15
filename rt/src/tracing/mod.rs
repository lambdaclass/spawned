//! Tracing initializer
//!

use std::str::FromStr;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::FmtSubscriber;
use tracing_subscriber::filter::Directive;

pub(crate) fn init_tracing() {
    let subscriber = FmtSubscriber::builder()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(Directive::from_str("info").unwrap())
                .from_env_lossy(),
        )
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
}
