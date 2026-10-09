//! Cached, local-only experiment selection, independent of profiler recording.

use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentationPolicy {
    Ordinary,
    ActionFull,
    Target,
    TargetAll,
    /// Action and selected-terminal early presentation share one budget.
    ActionFullTarget,
}

impl Default for PresentationPolicy {
    fn default() -> Self {
        if cfg!(feature = "latency-experiments") && crate::platform::latency_experiments_supported()
        {
            Self::ActionFullTarget
        } else {
            Self::Ordinary
        }
    }
}

impl PresentationPolicy {
    fn from_environment_value(value: Result<String, std::env::VarError>) -> io::Result<Self> {
        let policy = match value.as_deref() {
            Err(std::env::VarError::NotPresent) => Self::default(),
            Ok("ordinary") => Self::Ordinary,
            Ok("action-full") => Self::ActionFull,
            Ok("target") => Self::Target,
            Ok("target-all") => Self::TargetAll,
            Ok("action-full+target") => Self::ActionFullTarget,
            _ => return Err(invalid("unknown HERDR_LATENCY_PRESENTATION selector")),
        };
        if policy != Self::Ordinary {
            if !cfg!(feature = "latency-experiments") {
                return Err(invalid(
                    "latency selectors require latency-experiments feature",
                ));
            }
            if !crate::platform::latency_experiments_supported() {
                return Err(invalid("latency experiments are implemented only on Linux"));
            }
        }
        Ok(policy)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ExperimentConfig {
    pub(crate) presentation: PresentationPolicy,
}

impl ExperimentConfig {
    pub(crate) fn from_environment() -> io::Result<Self> {
        let presentation = PresentationPolicy::from_environment_value(std::env::var(
            "HERDR_LATENCY_PRESENTATION",
        ))?;
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
        tracing::debug!(?config.presentation, "latency experiment configuration");
        Ok(config)
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_selector_enables_supported_early_presentation() {
        let policy =
            PresentationPolicy::from_environment_value(Err(std::env::VarError::NotPresent))
                .expect("unset selector must allow startup");
        let mut presentation = crate::app::early_presentation::EarlyPresentation::new(policy);
        let enabled = cfg!(feature = "latency-experiments")
            && crate::platform::latency_experiments_supported();
        assert_eq!(presentation.actions_enabled(), enabled);
        assert_eq!(presentation.terminal_enabled(), enabled);
        presentation.accepted_action(
            "workspace".into(),
            "label".into(),
            "request".into(),
            std::time::Instant::now(),
        );
        assert_eq!(presentation.pending(), enabled);
    }

    #[test]
    fn ordinary_override_disables_both_early_paths() {
        let policy = PresentationPolicy::from_environment_value(Ok("ordinary".into()))
            .expect("ordinary override");
        let presentation = crate::app::early_presentation::EarlyPresentation::new(policy);
        assert!(!presentation.actions_enabled());
        assert!(!presentation.terminal_enabled());
    }

    #[test]
    fn explicit_early_policy_keeps_platform_and_feature_guards() {
        let policy = PresentationPolicy::from_environment_value(Ok("action-full+target".into()));
        if cfg!(feature = "latency-experiments") && crate::platform::latency_experiments_supported()
        {
            assert_eq!(
                policy.expect("supported policy"),
                PresentationPolicy::ActionFullTarget
            );
        } else {
            assert_eq!(
                policy.expect_err("unsupported policy").kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert!(PresentationPolicy::from_environment_value(Ok(String::new())).is_err());
    }
}
