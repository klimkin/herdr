//! Cached, local-only experiment selection, independent of profiler recording.

use std::io;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PresentationPolicy {
    #[default]
    Ordinary,
    ActionFull,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ExperimentConfig {
    pub(crate) presentation: PresentationPolicy,
}

impl ExperimentConfig {
    pub(crate) fn from_environment() -> io::Result<Self> {
        let presentation = match std::env::var("HERDR_LATENCY_PRESENTATION").as_deref() {
            Err(std::env::VarError::NotPresent) | Ok("ordinary") => PresentationPolicy::Ordinary,
            Ok("action-full") => PresentationPolicy::ActionFull,
            _ => return Err(invalid("unknown HERDR_LATENCY_PRESENTATION selector")),
        };
        for (name, control) in [
            ("HERDR_LATENCY_QUEUE_ORDER", "current"),
            ("HERDR_LATENCY_QUEUE_COUNT", "64"),
        ] {
            match std::env::var(name).as_deref() {
                Err(std::env::VarError::NotPresent) => {}
                Ok(value) if value == control => {}
                _ => {
                    return Err(invalid(
                        "queue experiments are not implemented in this ticket",
                    ))
                }
            }
        }
        let config = Self { presentation };
        if config.presentation != PresentationPolicy::Ordinary {
            if !cfg!(feature = "latency-experiments") {
                return Err(invalid(
                    "latency selectors require latency-experiments feature",
                ));
            }
            if !crate::platform::latency_experiments_supported() {
                return Err(invalid("latency experiments are implemented only on Linux"));
            }
        }
        tracing::debug!(?config.presentation, "latency experiment configuration");
        Ok(config)
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
