use std::future::Future;
use std::marker::PhantomData;

use crate::rollout::{RolloutDecision, RolloutStrategy};

/// An individual experiment. See crate-level documentation for an example on how
/// to use
pub struct Experiment<T, C, E, R, M> {
    result_type: PhantomData<T>,
    control_builder: C,
    experimental_builder: E,
    rollout_strategy: R,
    on_mismatch: M,
}

#[derive(Debug)]
/// Type passed to the `on_mismatch` function, which is called when the control
/// and experimental methods create different values.
pub struct Mismatch<T> {
    /// The value generated by the control method
    pub control: T,

    /// The value generated by the experimental method
    pub experimental: T,
}

impl<T> Experiment<T, (), (), (), Box<dyn FnOnce(Mismatch<T>) -> T>>
where
    T: PartialEq,
{
    /// Create a new experiment. The only provided default is accepting the
    /// control value in the mismatch handler. All other builder-style functions
    /// must be called before `run` can be called.
    pub fn new() -> Self {
        Self {
            result_type: PhantomData,
            control_builder: (),
            experimental_builder: (),
            on_mismatch: Box::new(|mismatch| mismatch.control),
            rollout_strategy: (),
        }
    }
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
            experimental_builder: self.experimental_builder,
            result_type: self.result_type,
            rollout_strategy: self.rollout_strategy,
            on_mismatch: self.on_mismatch,
        }
    }

    /// Use the future given here as the experimental, or the new method for
    /// calculating a value
    pub fn experimental<NE>(self, experiment_builder: NE) -> Experiment<T, C, NE, R, M>
    where
        NE: Future<Output = T>,
    {
        Experiment {
            experimental_builder: experiment_builder,
            result_type: self.result_type,
            control_builder: self.control_builder,
            rollout_strategy: self.rollout_strategy,
            on_mismatch: self.on_mismatch,
        }
    }

    /// Use the given strategy for rolling out the new code
    pub fn rollout_strategy<NR>(self, rollout_strategy: NR) -> Experiment<T, C, E, NR, M> {
        Experiment {
            rollout_strategy,
            result_type: self.result_type,
            control_builder: self.control_builder,
            experimental_builder: self.experimental_builder,
            on_mismatch: self.on_mismatch,
        }
    }

    /// Call this function when running the experiment results in a different
    /// value from the control and experimental methods. This can only happen
    /// when the rollout strategy returns
    /// `RolloutDecision::UseExperimentalAndCompare`.
    pub fn on_mismatch<NM>(self, on_mismatch: NM) -> Experiment<T, C, E, R, NM>
    where
        NM: FnOnce(Mismatch<T>) -> T,
    {
        Experiment {
            on_mismatch,
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
        M: FnOnce(Mismatch<T>) -> T,
        C: Future<Output = T>,
        E: Future<Output = T>,
    {
        match self.rollout_strategy.rollout_decision() {
            RolloutDecision::UseControl => self.control_builder.await,
            RolloutDecision::UseExperimental => self.experimental_builder.await,
            RolloutDecision::UseExperimentalAndCompare => {
                let (control, experimental) =
                    tokio::join!(self.control_builder, self.experimental_builder);

                if control != experimental {
                    let mismatch = Mismatch {
                        control,
                        experimental,
                    };

                    return (self.on_mismatch)(mismatch);
                }

                control
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn it_resolves_conflict_with_mismatch() {
        let mut experimental = true;

        let exists = Experiment::new()
            .control(async { true })
            .experimental(async {
                experimental = !experimental;
                experimental
            })
            .rollout_strategy(50.0)
            .on_mismatch(|mismatch| {
                assert_eq!(mismatch.control, true);
                assert_eq!(mismatch.experimental, false);

                mismatch.control
            })
            .run()
            .await;

        assert_eq!(exists, true);
    }
}
