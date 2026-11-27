//! Typed and raw incoming RPC handler traits for [`Client::provide`](crate::Client::provide)
//! and [`ManagedClientBuilder::provide`](crate::ManagedClientBuilder::provide).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::{ErrorCode, RpcError};
use crate::value::{ConversionError, FromParams, FromValue, IntoValue, Value};

/// Boxed `Send` future used for type-erased async RPC handlers.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Type-erased async RPC handler stored by [`Client`](crate::Client) and [`ManagedClient`](crate::ManagedClient).
pub type ErasedHandler = Arc<
    dyn Fn(Vec<Value>) -> BoxFuture<'static, std::result::Result<Value, RpcError>> + Send + Sync,
>;

impl From<ConversionError> for RpcError {
    fn from(err: ConversionError) -> Self {
        Self::new(ErrorCode::MalformedCall, err.message)
    }
}

/// Return type conversion for [`Handler`] callbacks.
///
/// Implemented automatically for any `T: IntoValue` (infallible handler) and
/// `Result<T, E>` where `T: IntoValue` and `E: Into<RpcError>` (fallible handler).
pub trait IntoHandlerResult {
    /// Convert the handler output into a MessagePack result value or wire [`RpcError`].
    fn into_handler_result(self) -> std::result::Result<Value, RpcError>;
}

impl<T: IntoValue> IntoHandlerResult for T {
    fn into_handler_result(self) -> std::result::Result<Value, RpcError> {
        self.try_into_value()
            .map_err(|err| RpcError::generic(err.message))
    }
}

impl<T: IntoValue, E: Into<RpcError>> IntoHandlerResult for std::result::Result<T, E> {
    fn into_handler_result(self) -> std::result::Result<Value, RpcError> {
        match self {
            Ok(v) => v
                .try_into_value()
                .map_err(|err| RpcError::generic(err.message)),
            Err(e) => Err(e.into()),
        }
    }
}

pub(crate) fn erase_raw_handler<F, Fut, R>(handler: F) -> ErasedHandler
where
    F: Fn(Vec<Value>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: IntoHandlerResult,
{
    let f = Arc::new(handler);
    Arc::new(move |params: Vec<Value>| {
        let f = Arc::clone(&f);
        Box::pin(async move { f(params).await.into_handler_result() })
    })
}

/// Async RPC callback accepting typed positional MessagePack arguments `Args`.
pub trait Handler<Args>: Send + Sync + 'static {
    /// Erase the typed handler signature into an [`ErasedHandler`].
    fn into_erased(self) -> ErasedHandler;
}

impl<F, Fut, R> Handler<()> for F
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = R> + Send + 'static,
    R: IntoHandlerResult,
{
    fn into_erased(self) -> ErasedHandler {
        let f = Arc::new(self);
        Arc::new(move |params: Vec<Value>| {
            let f = Arc::clone(&f);
            Box::pin(async move {
                <() as FromParams>::from_params(params).map_err(RpcError::from)?;
                f().await.into_handler_result()
            })
        })
    }
}

macro_rules! impl_handler_tuple {
    ($($name:ident),+ $(,)?) => {
        impl<F, Fut, R, $($name),+> Handler<($($name,)+)> for F
        where
            F: Fn($($name),+) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = R> + Send + 'static,
            R: IntoHandlerResult,
            $($name: FromValue + Send),+
        {
            fn into_erased(self) -> ErasedHandler {
                let f = Arc::new(self);
                Arc::new(move |params: Vec<Value>| {
                    let f = Arc::clone(&f);
                    Box::pin(async move {
                        #[allow(non_snake_case)]
                        let ($($name,)+) = <($($name,)+) as FromParams>::from_params(params)
                            .map_err(RpcError::from)?;
                        f($($name),+).await.into_handler_result()
                    })
                })
            }
        }
    };
}

impl_handler_tuple!(T0);
impl_handler_tuple!(T0, T1);
impl_handler_tuple!(T0, T1, T2);
impl_handler_tuple!(T0, T1, T2, T3);
impl_handler_tuple!(T0, T1, T2, T3, T4);
impl_handler_tuple!(T0, T1, T2, T3, T4, T5);
impl_handler_tuple!(T0, T1, T2, T3, T4, T5, T6);
impl_handler_tuple!(T0, T1, T2, T3, T4, T5, T6, T7);
