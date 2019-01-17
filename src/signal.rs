//! Signals are values that discretely change over time.

use crate::stream::Stream;
use crate::sync::Mutex;
use crate::types::{MaybeOwned, SharedChannel, SharedFold, SharedSignal};
use std::fmt;
use std::sync::{mpsc, Arc};

use self::SigValue::*;

/// Represents a discrete value that changes over time.
///
/// Signals are usually constructed by stream operations and can be read using the `sample` or
/// `take` methods. They update lazily when someone reads them.
#[derive(Clone, Debug)]
pub struct Signal<T>(SigValue<T>);

/// The content source of a signal.
#[derive(Clone)]
enum SigValue<T> {
    /// A signal with constant value.
    Constant(T),
    /// A signal that generates it's values from a function.
    ///
    /// This is produced by `Signal::from_fn`
    Dynamic(Arc<dyn Fn() -> T + Send + Sync>),
    /// A signal that contains shared data.
    ///
    /// This is mainly produced by stream methods or folding other signals.
    Shared(Arc<dyn SharedSignal<T> + Send + Sync>),
    /// A signal that contains a signal, and allows sampling the inner signal directly.
    ///
    /// This is produced by `Signal::switch`
    Nested(Arc<dyn Fn() -> Signal<T> + Send + Sync>),
}

impl<T> Signal<T> {
    /// Creates a signal with constant value.
    ///
    /// The value is assumed to be constant, so changing it while it's stored on the
    /// signal is a logic error and will cause unexpected results.
    pub const fn constant(val: T) -> Self {
        Signal(Constant(val))
    }

    /// Creates a signal that samples it's values from an external source.
    ///
    /// The closure is meant to sample a continuous value from the real world,
    /// so the signal value is assumed to be always changing.
    pub fn from_fn<F>(f: F) -> Self
    where
        F: Fn() -> T + Send + Sync + 'static,
    {
        Signal(Dynamic(Arc::new(f)))
    }

    /// Creates a new shared signal.
    pub(crate) fn shared<S>(storage: Arc<S>) -> Self
    where
        S: SharedSignal<T> + Send + Sync + 'static,
    {
        Signal(Shared(storage))
    }

    /// Samples the value of the signal.
    ///
    /// This will clone the value stored in the signal.
    pub fn sample(&self) -> T
    where
        T: Clone,
    {
        match self.0 {
            Constant(ref val) => T::clone(val),
            Dynamic(ref f) => f(),
            Shared(ref s) => s.sample().get(),
            Nested(ref f) => f().sample(),
        }
    }

    /// Maps a signal with the provided function.
    pub fn map<F, R>(&self, f: F) -> Signal<R>
    where
        F: Fn(T) -> R + Send + Sync + 'static,
        T: Clone + 'static,
        R: Send + 'static,
    {
        match self.0 {
            // constant signal: apply f once to produce another constant signal
            Constant(ref val) => Signal::constant(f(val.clone())),
            // dynamic signal: sample and apply f
            Dynamic(ref sf) => {
                let sf = sf.clone();
                Signal::from_fn(move || f(sf()))
            }
            // shared signal: sample and apply f
            Shared(ref sig) => {
                let sig = sig.clone();
                Signal::from_fn(move || f(sig.sample().get()))
            }
            // nested signal: extract signal, sample and apply f
            Nested(ref sf) => {
                let sf = sf.clone();
                Signal::from_fn(move || f(sf().sample()))
            }
        }
    }

    /// Folds a signal using the provided function.
    ///
    /// The folding operation will occur every time the signal is sampled.
    pub fn fold<A, F>(&self, initial: A, f: F) -> Signal<A>
    where
        F: Fn(A, T) -> A + Send + Sync + 'static,
        T: Clone + Send + 'static,
        A: Clone + Send + 'static,
    {
        match self.0 {
            Constant(ref val) => {
                let val = Mutex::new(val.clone()); // need Sync for T
                Signal::shared(SharedFold::new(initial, f, move || val.lock().clone()))
            }
            Dynamic(ref sf) => {
                let sf = sf.clone();
                Signal::shared(SharedFold::new(initial, f, move || sf()))
            }
            Shared(ref sig) => {
                let sig = sig.clone();
                Signal::shared(SharedFold::new(initial, f, move || sig.sample().get()))
            }
            Nested(ref sf) => {
                let sf = sf.clone();
                Signal::shared(SharedFold::new(initial, f, move || sf().sample()))
            }
        }
    }
}

impl<T: Send + 'static> Signal<T> {
    /// Samples the value of this signal every time the trigger stream fires.
    pub fn snapshot<S, F, R>(&self, trigger: &Stream<S>, f: F) -> Stream<R>
    where
        F: Fn(T, MaybeOwned<'_, S>) -> R + Send + Sync + 'static,
        T: Clone,
        S: 'static,
        R: 'static,
    {
        match self.0 {
            Constant(ref val) => {
                let val = Mutex::new(val.clone()); // need Sync for T
                trigger.map(move |t| f(val.lock().clone(), t))
            }
            Dynamic(ref sf) => {
                let sf = sf.clone();
                trigger.map(move |t| f(sf(), t))
            }
            Shared(ref sig) => {
                let sig = sig.clone();
                trigger.map(move |t| f(sig.sample().get(), t))
            }
            Nested(ref sf) => {
                let sf = sf.clone();
                trigger.map(move |t| f(sf().sample(), t))
            }
        }
    }

    /// Stores the last value sent to a channel.
    ///
    /// When sampled, the resulting signal consumes all the current values on the channel
    /// (using `try_recv`) and returns the last value seen.
    #[inline]
    pub fn from_channel(initial: T, rx: mpsc::Receiver<T>) -> Self {
        Self::fold_channel(initial, rx, |_, v| v)
    }

    /// Creates a signal that folds the values from a channel.
    ///
    /// When sampled, the resulting signal consumes all the current values on the channel
    /// (using `try_recv`) and folds them using the current signal value as the
    /// initial accumulator state.
    pub fn fold_channel<V, F>(initial: T, rx: mpsc::Receiver<V>, f: F) -> Self
    where
        F: Fn(T, V) -> T + Send + Sync + 'static,
        V: Send + 'static,
    {
        Signal::shared(SharedChannel::new(initial, rx, f))
    }
}

impl<T: Clone + 'static> Signal<Signal<T>> {
    /// Creates a new signal that samples the inner value of a nested signal.
    pub fn switch(&self) -> Signal<T> {
        match self.0 {
            // constant signal: just extract the inner signal
            Constant(ref sig) => Signal::clone(sig),
            // dynamic signal: re-label as nested
            Dynamic(ref f) => Signal(Nested(f.clone())),
            // shared signal: sample to extract the inner signal
            Shared(ref sig) => {
                let sig = sig.clone();
                Signal(Nested(Arc::new(move || sig.sample().get())))
            }
            // nested signal: remove one layer
            Nested(ref f) => {
                let f = f.clone();
                Signal(Nested(Arc::new(move || f().sample())))
            }
        }
    }
}

impl<T: Default> Default for Signal<T> {
    /// Creates a constant signal with T's default value.
    #[inline]
    fn default() -> Self {
        Signal::constant(T::default())
    }
}

impl<T> From<T> for Signal<T> {
    /// Creates a constant signal from T.
    #[inline]
    fn from(val: T) -> Self {
        Signal::constant(val)
    }
}

impl<T: fmt::Debug> fmt::Debug for SigValue<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Constant(ref val) => write!(f, "Constant({:?})", val),
            Dynamic(ref rf) => write!(f, "Dynamic(Fn@{:p})", rf),
            Shared(ref rs) => write!(f, "Shared(SharedSignal@{:p})", rs),
            Nested(ref rf) => write!(f, "Nested(Fn@{:p})", rf),
        }
    }
}

impl<T: fmt::Display + Clone> fmt::Display for Signal<T> {
    /// Samples the signal and formats the value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.sample(), f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SharedStorage;
    use std::sync::RwLock;
    use std::time::Instant;

    #[test]
    fn signal_basic() {
        let signal = Signal::constant(42);
        let double = signal.map(|a| a * 2);
        let plusone = double.map(|a| a + 1);
        assert_eq!(signal.sample(), 42);
        assert_eq!(double.sample(), 84);
        assert_eq!(plusone.sample(), 85);
    }

    #[test]
    fn signal_shared() {
        let st = Arc::new(SharedStorage::new(1, ()));
        let signal = Signal::shared(st.clone());
        let double = signal.map(|a| a * 2);

        assert_eq!(signal.sample(), 1);
        assert_eq!(double.sample(), 2);
        st.set(42);
        assert_eq!(signal.sample(), 42);
        assert_eq!(double.sample(), 84);
    }

    #[test]
    fn signal_dynamic() {
        let t = Instant::now();
        let signal = Signal::from_fn(move || t);
        assert_eq!(signal.sample(), t);

        let n = Arc::new(RwLock::new(1));
        let cloned = n.clone();
        let signal = Signal::from_fn(move || *cloned.read().unwrap());
        let double = signal.map(|a| a * 2);
        let plusone = double.map(|a| a + 1);
        assert_eq!(signal.sample(), 1);
        assert_eq!(double.sample(), 2);
        assert_eq!(plusone.sample(), 3);
        *n.write().unwrap() = 13;
        assert_eq!(signal.sample(), 13);
        assert_eq!(double.sample(), 26);
        assert_eq!(plusone.sample(), 27);
    }

    #[test]
    fn signal_fold() {
        let sig1 = Signal::constant(1).fold(0, |a, n| a + n);
        let sig2 = Signal::from_fn(|| 1).fold(0, |a, n| a + n);
        let sig3 = Signal::shared(Arc::new(SharedStorage::new(1, ()))).fold(0, |a, n| a + n);

        assert_eq!(sig1.sample(), 1);
        assert_eq!(sig2.sample(), 1);
        assert_eq!(sig3.sample(), 1);

        assert_eq!(sig1.sample(), 2);
        assert_eq!(sig2.sample(), 2);
        assert_eq!(sig3.sample(), 2);
    }

    #[test]
    fn signal_const() {
        const THE_ANSWER: Signal<i32> = Signal::constant(42);

        assert_eq!(THE_ANSWER.sample(), 42);
    }
}
