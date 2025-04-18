use metrics::{counter, histogram};
use std::fmt::Display;
use std::future::Future;
use std::marker::PhantomData;
use std::time::Instant;
use tracing::{info_span, Instrument};

use crate::mismatch::{self, Mismatch, MismatchHandler};
use crate::rollout::{RolloutDecision, RolloutStrategy};

/// An individual experiment. See crate-level documentation for an example on how
/// to use
pub struct Experiment<T, C, E, R, M> {
    result_type: PhantomData<T>,
    control_builder: C,
    experimental_builder: E,
    rollout_strategy: R,
    mismatch_handler: M,
    name: &'static str,
}

impl<T> Experiment<T, (), (), (), mismatch::AlwaysControl> {
    /// Create a new experiment. The only provided default is accepting the
    /// control value in the mismatch handler. All other builder-style functions
    /// must be called before `run` can be called.
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            result_type: PhantomData,
            control_builder: (),
            experimental_builder: (),
            mismatch_handler: mismatch::AlwaysControl,
            rollout_strategy: (),
        }
    }
}

async fn instrument_control<F, T>(name: &'static str, future: F) -> T
where
    F: Future<Output = T>,
{
    measure_duration(
        name,
        "control",
        future.instrument(info_span!("Experiment::run control", method = "control")),
    )
    .await
}

async fn instrument_experimental<F, T>(name: &'static str, future: F) -> T
where
    F: Future<Output = T>,
{
    measure_duration(
        name,
        "experimental",
        future.instrument(info_span!(
            "Experiment::run experimental",
            method = "experimental"
        )),
    )
    .await
}

async fn measure_duration<F, T>(name: &'static str, kind: &'static str, future: F) -> T
where
    F: Future<Output = T>,
{
    let start = Instant::now();
    let output = future.await;
    let duration = start.elapsed();

    histogram!(
        "thesis_experiment_run_duration",
        "name" => name,
        "kind" => kind,
    )
    .record(duration);

    output
}

impl<T, C, E, R, M> Experiment<T, C, E, R, M> {
    /// Use the future given here as the control, or the existing method for
    /// calculating a value
    pub fn control<NC>(self, control_builder: NC) -> Experiment<T, NC, E, R, M>
    where
        NC: Future<Output = T>,
    {
        Experiment {
            control_builder,
            name: self.name,
            experimental_builder: self.experimental_builder,
            result_type: self.result_type,
            rollout_strategy: self.rollout_strategy,
            mismatch_handler: self.mismatch_handler,
        }
    }

    /// Use the future given here as the experimental, or the new method for
    /// calculating a value
    pub fn experimental<NE>(self, experimental_builder: NE) -> Experiment<T, C, NE, R, M>
    where
        NE: Future<Output = T>,
    {
        Experiment {
            experimental_builder,
            name: self.name,
            result_type: self.result_type,
            control_builder: self.control_builder,
            rollout_strategy: self.rollout_strategy,
            mismatch_handler: self.mismatch_handler,
        }
    }

    /// Use the given strategy for rolling out the new code
    pub fn rollout_strategy<NR>(self, rollout_strategy: NR) -> Experiment<T, C, E, NR, M> {
        Experiment {
            rollout_strategy,
            name: self.name,
            result_type: self.result_type,
            control_builder: self.control_builder,
            experimental_builder: self.experimental_builder,
            mismatch_handler: self.mismatch_handler,
        }
    }

    /// Call this function when running the experiment results in a different
    /// value from the control and experimental methods. This can only happen
    /// when the rollout strategy returns
    /// `RolloutDecision::UseExperimentalAndCompare`.
    pub fn on_mismatch<NM>(self, on_mismatch: NM) -> Experiment<T, C, E, R, mismatch::FnTrait<NM>>
    where
        NM: FnOnce(Mismatch<T>) -> T,
    {
        Experiment {
            mismatch_handler: mismatch::FnTrait(on_mismatch),
            name: self.name,
            rollout_strategy: self.rollout_strategy,
            result_type: self.result_type,
            control_builder: self.control_builder,
            experimental_builder: self.experimental_builder,
        }
    }

    /// Run the experiment with the parameters provided
    pub async fn run(self) -> T
    where
        T: PartialEq,
        R: RolloutStrategy,
        M: MismatchHandler<T>,
        C: Future<Output = T>,
        E: Future<Output = T>,
    {
        let span = info_span!("Experiment::run", experiment_name = self.name);
        counter!("thesis_experiment_run_total", "name" => self.name).increment(1);

        async move {
            match self.rollout_strategy.rollout_decision() {
                RolloutDecision::UseControl => {
                    counter!(
                        "thesis_experiment_run_variant",
                        "name" => self.name,
                        "kind" => "control",
                    )
                    .increment(1);

                    instrument_control(self.name, self.control_builder).await
                }
                RolloutDecision::UseExperimentalAndCompare => {
                    counter!(
                        "thesis_experiment_run_variant",
                        "name" => self.name,
                        "kind" => "experimental_and_compare",
                    )
                    .increment(1);

                    let (control, experimental) = tokio::join!(
                        instrument_control(self.name, self.control_builder),
                        instrument_experimental(self.name, self.experimental_builder),
                    );

                    if control != experimental {
                        outcome_mismatch(self.name);

                        let mismatch = Mismatch {
                            control,
                            experimental,
                        };

                        return self.mismatch_handler.on_mismatch(mismatch);
                    }

                    control
                }
                RolloutDecision::UseExperimental => {
                    counter!(
                        "thesis_experiment_run_variant",
                        "name" => self.name,
                        "kind" => "experimental",
                    )
                    .increment(1);

                    instrument_experimental(self.name, self.experimental_builder).await
                }
            }
        }
        .instrument(span)
        .await
    }
}

fn outcome_error<E>(name: &'static str, kind: &'static str, error: &E)
where
    E: Display,
{
    counter!(
        "thesis_experiment_outcome",
        "name" => name,
        "kind" => kind,
        "outcome" => "error",
    )
    .increment(1);

    tracing::error!(name, kind, %error, "thesis experiment error");
}

fn outcome_ok(name: &'static str, kind: &'static str) {
    counter!(
        "thesis_experiment_outcome",
        "name" => name,
        "kind" => kind,
        "outcome" => "ok",
    )
    .increment(1);
}

fn outcome_mismatch(name: &'static str) {
    counter!(
        "thesis_experiment_outcome",
        "name" => name,
        "kind" => "experimental_and_compare",
        "outcome" => "mismatch",
    )
    .increment(1);
}

fn outcome<T, E>(name: &'static str, kind: &'static str, result: &Result<T, E>)
where
    E: Display,
{
    match result {
        Ok(_) => {
            outcome_ok(name, kind);
        }
        Err(e) => {
            outcome_error(name, kind, e);
        }
    }
}

impl<T, Err, C, E, R, M> Experiment<Result<T, Err>, C, E, R, M> {
    /// Run the experiment with the parameters provided
    pub async fn run_result(self) -> Result<T, Err>
    where
        T: PartialEq,
        R: RolloutStrategy,
        M: MismatchHandler<Result<T, Err>>,
        C: Future<Output = Result<T, Err>>,
        E: Future<Output = Result<T, Err>>,
        Err: Display,
    {
        let span = info_span!("Experiment::run", experiment_name = self.name);
        counter!("thesis_experiment_run_total", "name" => self.name).increment(1);

        async move {
            match self.rollout_strategy.rollout_decision() {
                RolloutDecision::UseControl => {
                    counter!(
                        "thesis_experiment_run_variant",
                        "name" => self.name,
                        "kind" => "control",
                    )
                    .increment(1);

                    let result = instrument_control(self.name, self.control_builder).await;
                    outcome(self.name, "control", &result);

                    result
                }
                RolloutDecision::UseExperimentalAndCompare => {
                    counter!(
                        "thesis_experiment_run_variant",
                        "name" => self.name,
                        "kind" => "experimental_and_compare",
                    )
                    .increment(1);

                    let (control, experimental) = tokio::join!(
                        instrument_control(self.name, self.control_builder),
                        instrument_experimental(self.name, self.experimental_builder)
                    );

                    outcome(self.name, "control", &control);
                    outcome(self.name, "experimental", &experimental);

                    match (control, experimental) {
                        (Ok(control), Ok(experimental)) => {
                            if control != experimental {
                                outcome_mismatch(self.name);

                                let mismatch = Mismatch {
                                    control: Ok(control),
                                    experimental: Ok(experimental),
                                };

                                return self.mismatch_handler.on_mismatch(mismatch);
                            }

                            Ok(control)
                        }
                        (Ok(control), Err(_)) => {
                            outcome_mismatch(self.name);

                            Ok(control)
                        }
                        (Err(control), Ok(experimental)) => {
                            outcome_mismatch(self.name);

                            let mismatch = Mismatch {
                                control: Err(control),
                                experimental: Ok(experimental),
                            };

                            self.mismatch_handler.on_mismatch(mismatch)
                        }
                        (Err(control), Err(_)) => Err(control),
                    }
                }
                RolloutDecision::UseExperimental => {
                    counter!(
                        "thesis_experiment_run_variant",
                        "name" => self.name,
                        "kind" => "experimental",
                    )
                    .increment(1);

                    let result =
                        instrument_experimental(self.name, self.experimental_builder).await;
                    outcome(self.name, "experimental", &result);

                    result
                }
            }
        }
        .instrument(span)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollout::Percent;

    #[tokio::test]
    async fn it_resolves_conflict_with_mismatch() {
        let mut experimental = true;

        let exists = Experiment::new("test")
            .control(async { true })
            .experimental(async {
                experimental = !experimental;
                experimental
            })
            .rollout_strategy(Percent::new(50.0))
            .on_mismatch(|mismatch| {
                assert!(mismatch.control);
                assert!(!mismatch.experimental);

                mismatch.control
            })
            .run()
            .await;

        assert!(exists);
    }

    #[tokio::test]
    async fn it_rolls_out_correctly() {
        let mut trues = 0;
        let mut falses = 0;

        for _ in 0..10_000usize {
            let exists = Experiment::new("test")
                .control(async { true })
                .experimental(async { false })
                .rollout_strategy(Percent::new(5.0))
                .on_mismatch(|mismatch| mismatch.experimental)
                .run()
                .await;

            if exists {
                trues += 1;
            } else {
                falses += 1;
            }
        }

        let experimental_rate = falses as f64 / (trues + falses) as f64;

        // Actual rate will be calculated via RNG, should be .04, .05, or .06.
        assert!(
            0.04 < experimental_rate && experimental_rate < 0.07,
            "rate of experimental was {}",
            experimental_rate
        );
    }

    #[tokio::test]
    async fn it_works_with_results() {
        let exists = Experiment::new("test")
            .control(async { Ok::<_, &str>(true) })
            .experimental(async { Ok::<_, &str>(false) })
            .rollout_strategy(RolloutDecision::UseControl)
            .run_result()
            .await;

        assert_eq!(exists, Ok(true));
    }

    #[test]
    fn test_experiment_is_send() {
        fn is_send(_: impl Send) {}

        is_send(
            Experiment::new("test")
                .rollout_strategy(RolloutDecision::UseControl)
                .control(async {}),
        );
    }

    #[tokio::test]
    async fn it_falls_back_to_control_when_experimental_fails() {
        let mut seen = false;
        let exists = Experiment::new("test")
            .control(async { Ok::<_, &str>(true) })
            .experimental(async {
                seen = true;
                Err::<bool, &str>("failed")
            })
            .rollout_strategy(RolloutDecision::UseExperimentalAndCompare)
            .run_result()
            .await;

        assert_eq!(exists, Ok(true));
        assert!(seen);
    }

    #[tokio::test]
    async fn it_calls_mismatch_when_control_errs_and_experiment_is_ok() {
        let mut seen = false;
        let exists = Experiment::new("test")
            .control(async { Err::<bool, &str>("failed") })
            .experimental(async { Ok::<_, &str>(true) })
            .rollout_strategy(RolloutDecision::UseExperimentalAndCompare)
            .on_mismatch(|m| {
                seen = true;

                m.experimental
            })
            .run_result()
            .await;

        assert_eq!(exists, Ok(true));
        assert!(seen);
    }

    #[tokio::test]
    async fn it_works_with_non_partialeq_errs() {
        #[derive(Debug)]
        struct NonPartialEq;

        impl Display for NonPartialEq {
            fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(fmt, "NonPartialEq")
            }
        }

        let mut seen = false;
        let exists = Experiment::new("test")
            .control(async { Err::<bool, NonPartialEq>(NonPartialEq) })
            .experimental(async { Ok::<_, NonPartialEq>(true) })
            .rollout_strategy(RolloutDecision::UseExperimentalAndCompare)
            .on_mismatch(|m| {
                seen = true;

                m.experimental
            })
            .run_result()
            .await;

        match exists {
            Ok(true) => {}
            x => panic!("Unexpected result: {:?}", x),
        }

        assert!(seen);
    }

    #[tokio::test]
    async fn it_runs_experimental_result_and_ignores_control() {
        let mut seen = false;
        let exists = Experiment::new("test")
            .control(async {
                seen = true;
                Err::<bool, &str>("failed")
            })
            .experimental(async { Ok::<_, &str>(true) })
            .rollout_strategy(RolloutDecision::UseExperimental)
            .run_result()
            .await;

        assert_eq!(exists, Ok(true));
        assert!(!seen);
    }

    #[tokio::test]
    async fn it_runs_experimental_and_ignores_control() {
        let mut seen = false;
        let exists = Experiment::new("test")
            .control(async {
                seen = true;
                false
            })
            .experimental(async { true })
            .rollout_strategy(RolloutDecision::UseExperimental)
            .run()
            .await;

        assert!(exists);
        assert!(!seen);
    }
}
