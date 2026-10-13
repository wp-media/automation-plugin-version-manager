//! JS built-ins captured when the addon loads, so later user code that
//! replaces them cannot change what the addon does with them.
//!
//! Today: `Promise.prototype.then`, used to handle the rejection of a
//! promise `SiteInventory.update()` refuses
//! ([`crate::sites_inventory`]). Calling `promise.catch(…)` instead would
//! run whatever `catch` the promise has: a subclass overriding it to throw
//! left the rejection unhandled, and Node's default policy then ends the
//! process.
//!
//! What this covers and what it cannot:
//!
//! - Captured per JS environment (the main thread and each worker), in
//!   that environment's instance data, when the addon is loaded there
//!   (`require`). Code that replaced `Promise` or `Promise.prototype.then`
//!   *before* the addon loaded is captured as it is.
//! - The intrinsic `then` ignores a `then` or `catch` a promise subclass
//!   overrides. It still constructs the derived promise through the
//!   promise's species constructor (`constructor[Symbol.species]`), which
//!   is the subclass's own code: if that throws, the handler is not
//!   attached and the caller clears the exception (best effort).

use napi::bindgen_prelude::{
    FnArgs, FromNapiValue, Function, FunctionRef, JsObjectValue, JsValue, Object, Unknown,
};
use napi::{Env, Status};

/// A captured function, typed only for storage: [`Intrinsics::promise_then`]
/// re-types it for the call.
type StoredFunction = FunctionRef<(), ()>;

/// The built-ins captured for one JS environment.
struct Intrinsics {
    /// `Promise.prototype.then`.
    promise_then: StoredFunction,
    /// A function that ignores its arguments, used as a rejection handler.
    ignore: StoredFunction,
}

/// Capture the built-ins of the environment `env` (called once per
/// environment, when the addon is loaded there). Replaces anything
/// captured earlier in that environment.
///
/// # Errors
///
/// When `Promise.prototype.then` cannot be read, or a reference or the
/// instance data cannot be created; JS may then have an exception pending.
pub(crate) fn capture(env: &Env) -> napi::Result<()> {
    // `Promise` is a function: read it untyped, then as an object.
    let promise: Unknown<'_> = env.get_global()?.get_named_property("Promise")?;
    let prototype: Object<'_> = promise
        .coerce_to_object()?
        .get_named_property("prototype")?;
    let then: Function<'_, (), ()> = prototype.get_named_property("then")?;
    let ignore: Function<'_, (), ()> = env.create_function_from_closure("ignore", |_| Ok(()))?;
    let intrinsics = Intrinsics {
        promise_then: then.create_ref()?,
        ignore: ignore.create_ref()?,
    };
    // The references are released with the environment's instance data.
    env.set_instance_data(intrinsics, (), |_| {})
}

/// Handle the rejection of `promise` (ignore it), with the captured
/// `Promise.prototype.then`: `then.call(promise, undefined, ignore)`.
///
/// # Errors
///
/// When nothing was captured in this environment, or the call throws (the
/// promise's species constructor is user code; the exception is then left
/// pending for the caller to clear).
pub(crate) fn ignore_rejection(env: &Env, promise: Unknown<'_>) -> napi::Result<()> {
    let Some(intrinsics) = env.get_instance_data::<Intrinsics>()? else {
        return Err(napi::Error::new(
            Status::GenericFailure,
            "Promise.prototype.then was not captured when the addon loaded",
        ));
    };
    let then = intrinsics.promise_then(env)?;
    let ignore = intrinsics.ignore.borrow_back(env)?.to_unknown();
    then.apply(promise, ((), ignore).into())?;
    Ok(())
}

impl Intrinsics {
    /// The captured `Promise.prototype.then`, typed for
    /// `then(undefined, onRejected)`.
    ///
    /// # Errors
    ///
    /// When the reference cannot be read back.
    fn promise_then<'env>(
        &self,
        env: &'env Env,
    ) -> napi::Result<Function<'env, FnArgs<((), Unknown<'env>)>, Unknown<'env>>> {
        let then = self.promise_then.borrow_back(env)?.to_unknown();
        Function::from_unknown(then)
    }
}
