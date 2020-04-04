//! See the [README](https://github.com/Ekleog/rlua-async) for more general information
// TODO: uncomment when stable #![doc(include = "../README.md")]

// # Implementation details note
//
// Having a `Poll::Pending` returned from a Rust `async` function will trigger a
// `coroutine.yield()`, with no arguments and that doesn't use its return value.
// This `coroutine.yield()` will bubble up to the closest `create_thread` Rust call (assuming the
// Lua code doesn't use coroutines in-between, which would break all hell loose).

use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{self, Poll},
};

use futures::future;
use rlua::*;
use scoped_tls::scoped_thread_local;

/// A "prelude" that provides all the extension traits that need to be in scope for the
/// `async`-related functions to be usable.
pub mod prelude {
    pub use super::{ContextExt, FunctionExt};
}

// Safety invariant: This always points to a valid `task::Context`.
// This is guaranteed by the fact that it is *always* set with
// `FUTURE_CTX.set(&(ctx as *mut _ as *mut ()), ...`, meaning that the `ctx` lifetime cannot be
// less than the time this pointer is in here.
// TODO: Is there a way to not be that awful? I don't think so, because:
//  * we can't just store an `&mut` in a thread-local for lack of lifetime, and
//  * we can't clone the `Context`, as it's not `Clone`
scoped_thread_local!(static FUTURE_CTX: *mut ());

/// Extension trait for [`rlua::Context`]
pub trait ContextExt<'lua> {
    /// Create an asynchronous function.
    ///
    /// This works exactly like [`Context::create_function`], except that the function returns a
    /// [`Future`] instead of just the result. Note that when this function is being called from
    /// Lua, it will generate a coroutine, that will prevent any use of coroutines in the said Lua
    /// code and is designed to be called from an `async`-compliant caller such as
    /// [`FunctionExt::call_async`]
    fn create_async_function<Arg, Ret, RetFut, F>(self, func: F) -> Result<Function<'lua>>
    where
        Arg: FromLuaMulti<'lua>,
        Ret: ToLua<'lua>,
        RetFut: 'static + Send + Future<Output = Result<Ret>>,
        F: 'static + Send + Fn(Context<'lua>, Arg) -> RetFut;
}

impl<'lua> ContextExt<'lua> for Context<'lua> {
    fn create_async_function<Arg, Ret, RetFut, F>(self, func: F) -> Result<Function<'lua>>
    where
        Arg: FromLuaMulti<'lua>,
        Ret: ToLuaMulti<'lua>,
        // TODO: 'static below should probably be 'lua instead -- need to figure out a way to work
        // around the 'static bound on create_function
        RetFut: 'static + Send + Future<Output = Result<Ret>>,
        F: 'static + Send + Fn(Context<'lua>, Arg) -> RetFut,
    {
        let wrapped_fun = self.create_function(move |ctx, arg| {
            let mut fut = Box::pin(func(ctx, arg)); // TODO: maybe we can avoid this pin?
            ctx.create_function_mut(move |ctx, _: MultiValue<'lua>| {
                FUTURE_CTX.with(|fut_ctx| {
                    let fut_ctx_ref = unsafe { &mut *(*fut_ctx as *mut task::Context) };
                    match Future::poll(fut.as_mut(), fut_ctx_ref) {
                        Poll::Pending => ToLuaMulti::to_lua_multi((rlua::Value::Nil, false), ctx),
                        Poll::Ready(v) => {
                            let v = ToLuaMulti::to_lua_multi(v?, ctx)?.into_vec();
                            ToLuaMulti::to_lua_multi((v, true), ctx)
                        }
                    }
                })
            })
        })?;

        self.load(
            r#"
                function(f)
                    return function(...)
                        local poll = f(...)
                        while true do
                            local t, ready = poll()
                            if ready then
                                return table.unpack(t)
                            else
                                coroutine.yield()
                            end
                        end
                    end
                end
            "#,
        )
        .set_name(b"coroutine yield helper")?
        .eval::<Function<'lua>>()? // TODO: find some way to cache this eval, maybe?
        .call(wrapped_fun)
    }
}

struct PollThreadFut<'lua, Arg, Ret> {
    /// If set to Some(a), contains the arguments that will be passed at the first resume, ie. the
    /// function arguments
    args: Option<Arg>,
    ctx: Context<'lua>,
    thread: Thread<'lua>,
    _phantom: PhantomData<Ret>,
}

impl<'lua, Arg, Ret> Future for PollThreadFut<'lua, Arg, Ret>
where
    Arg: ToLuaMulti<'lua>,
    Ret: FromLuaMulti<'lua>,
{
    type Output = Result<Ret>;

    fn poll(mut self: Pin<&mut Self>, fut_ctx: &mut task::Context) -> Poll<Result<Ret>> {
        FUTURE_CTX.set(&(fut_ctx as *mut _ as *mut ()), || {
            let taken_args = unsafe { self.as_mut().get_unchecked_mut().args.take() };

            let resume_ret = if let Some(a) = taken_args {
                self.thread.resume::<_, rlua::MultiValue>(a)
            } else {
                self.thread.resume::<_, rlua::MultiValue>(())
            };

            match resume_ret {
                Err(e) => Poll::Ready(Err(e)),
                Ok(v) => {
                    match self.thread.status() {
                        ThreadStatus::Resumable => Poll::Pending,

                        ThreadStatus::Unresumable => {
                            Poll::Ready(FromLuaMulti::from_lua_multi(v, self.ctx))
                        }

                        // The `Error` case should be caught by the `Err(e)` match above
                        ThreadStatus::Error => unreachable!(),
                    }
                }
            }
        })
    }
}

/// Extension trait for [`rlua::Function`]
pub trait FunctionExt<'lua> {
    /// Calls the function in an async-compliant way.
    ///
    /// By using this on the Rust side, you can recover as a [`Future`] the potentiall
    /// [`Poll::Pending`] that might have been sent by eg. a downstream
    /// [`ContextExt::create_async_function`]
    // TODO: make the return type `impl trait`... when GAT + existential types will be stable?
    fn call_async<'fut, Arg, Ret>(
        &self,
        ctx: Context<'lua>,
        args: Arg,
    ) -> Pin<Box<dyn Future<Output = Result<Ret>> + 'fut>>
    where
        'lua: 'fut,
        Arg: 'fut + ToLuaMulti<'lua>,
        Ret: 'fut + FromLuaMulti<'lua>;
}

impl<'lua> FunctionExt<'lua> for Function<'lua> {
    fn call_async<'fut, Arg, Ret>(
        &self,
        ctx: Context<'lua>,
        args: Arg,
    ) -> Pin<Box<dyn Future<Output = Result<Ret>> + 'fut>>
    where
        'lua: 'fut,
        Arg: 'fut + ToLuaMulti<'lua>,
        Ret: 'fut + FromLuaMulti<'lua>,
    {
        let thread = match ctx.create_thread(self.clone()) {
            Ok(thread) => thread,
            Err(e) => return Box::pin(future::err(e)),
        };

        Box::pin(PollThreadFut {
            args: Some(args),
            ctx,
            thread,
            _phantom: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use futures::executor;

    #[test]
    fn it_works() {
        let lua = Lua::new();

        lua.context(|lua_ctx| {
            let globals = lua_ctx.globals();

            let f = lua_ctx
                .create_async_function(|_, a: usize| future::ok(a + 1))
                .unwrap();
            globals.set("f", f).unwrap();

            assert_eq!(
                executor::block_on(
                    lua_ctx
                        .load(r#"function(a) return f(a) - 1 end"#)
                        .eval::<Function>()
                        .unwrap()
                        .call_async::<_, usize>(lua_ctx, 2)
                )
                .unwrap(),
                2
            );
        });
    }

    #[test]
    fn actually_doing_things() {
        let lua = Lua::new();

        lua.context(|lua_ctx| {
            let globals = lua_ctx.globals();

            let f = lua_ctx
                .create_async_function(|_, a: usize| async move {
                    futures_timer::Delay::new(Duration::from_millis(50)).await;
                    Ok(a + 1)
                })
                .unwrap();
            globals.set("f", f).unwrap();

            assert_eq!(
                executor::block_on(
                    lua_ctx
                        .load(r#"function(a) return f(a) - 1 end"#)
                        .set_name(b"example")
                        .expect("failed to set name")
                        .eval::<Function>()
                        .expect("failed to eval")
                        .call_async::<_, usize>(lua_ctx, 2)
                )
                .expect("failed to call"),
                2
            );
        });
    }
}
