use std::os::raw::c_char;

use mullvad_api::{
    AccountsProxy,
    rest::{self, MullvadRestHandle},
};
use mullvad_types::account::AccountNumber;

use super::{
    SwiftApiContext,
    cancellation::{RequestCancelHandle, SwiftCancelHandle},
    completion::{CompletionCookie, SwiftCompletionHandler},
    do_request, get_string,
    response::SwiftMullvadApiResponse,
    retry_strategy::{RetryStrategy, SwiftRetryStrategy},
};

/// # Safety
///
/// `api_context` must be pointing to a valid instance of `SwiftApiContext`. A `SwiftApiContext` is created
/// by calling `mullvad_api_init_new`.
///
/// This function takes ownership of `completion_cookie`, which must be pointing to a valid instance of Swift
/// object `MullvadApiCompletion`. The pointer will be freed by calling `mullvad_api_completion_finish`
/// when completion finishes (in completion.finish).
///
/// `retry_strategy` must have been created by a call to either of the following functions
/// `mullvad_api_retry_strategy_never`, `mullvad_api_retry_strategy_constant` or `mullvad_api_retry_strategy_exponential`
///
/// `account_number` must be a pointer to a null terminated string.
///
/// `body` must be a pointer to a contiguous memory segment
///
/// `body_size` must be the size of the body
///
/// This function is not safe to call multiple times with the same `CompletionCookie`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mullvad_ios_legacy_storekit_payment(
    api_context: SwiftApiContext,
    completion_cookie: *mut libc::c_void,
    retry_strategy: SwiftRetryStrategy,
    account_number: *const c_char,
    body: *const u8,
    body_size: usize,
) -> SwiftCancelHandle {
    let completion_handler =
        SwiftCompletionHandler::new(unsafe { CompletionCookie::new(completion_cookie) });

    let Ok(tokio_handle) = crate::mullvad_ios_runtime() else {
        completion_handler.finish(SwiftMullvadApiResponse::no_tokio_runtime());
        return SwiftCancelHandle::empty();
    };

    let api_context = api_context.rust_context();
    // SAFETY: See SwiftRetryStrategy::into_rust.
    let retry_strategy = unsafe { retry_strategy.into_rust() };

    let completion = completion_handler.clone();

    // SAFETY: See param documentation for `account_number`.
    let account_number = AccountNumber::from(unsafe { get_string(account_number) });

    // SAFETY: See param documentation for `body`.
    let body = unsafe { std::slice::from_raw_parts(body, body_size) }.to_vec();
    let task = tokio_handle.spawn(async move {
        match mullvad_ios_legacy_storekit_payment_inner(
            api_context.rest_handle(),
            retry_strategy,
            account_number,
            body,
        )
        .await
        {
            Ok(response) => completion.finish(response),
            Err(err) => {
                log::error!("{err:?}");
                completion.finish(SwiftMullvadApiResponse::rest_error(err));
            }
        }
    });

    RequestCancelHandle::new(task, completion_handler.clone()).into_swift()
}

async fn mullvad_ios_legacy_storekit_payment_inner(
    rest_client: MullvadRestHandle,
    retry_strategy: RetryStrategy,
    account_number: AccountNumber,
    body: Vec<u8>,
) -> Result<SwiftMullvadApiResponse, rest::Error> {
    let account_proxy = AccountsProxy::new(rest_client);

    let future_factory =
        || account_proxy.legacy_storekit_payment(account_number.clone(), body.clone());

    do_request(retry_strategy, future_factory).await
}

/// # Safety
///
/// `api_context` must be pointing to a valid instance of `SwiftApiContext`. A `SwiftApiContext` is created
/// by calling `mullvad_api_init_new`.
///
/// This function takes ownership of `completion_cookie`, which must be pointing to a valid instance of Swift
/// object `MullvadApiCompletion`. The pointer will be freed by calling `mullvad_api_completion_finish`
/// when completion finishes (in completion.finish).
///
/// `account_number` must be a pointer to a null terminated string.
///
/// `retry_strategy` must have been created by a call to either of the following functions
/// `mullvad_api_retry_strategy_never`, `mullvad_api_retry_strategy_constant` or `mullvad_api_retry_strategy_exponential`
///
/// This function is not safe to call multiple times with the same `CompletionCookie`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mullvad_ios_init_storekit_payment(
    api_context: SwiftApiContext,
    completion_cookie: *mut libc::c_void,
    retry_strategy: SwiftRetryStrategy,
    account_number: *const c_char,
) -> SwiftCancelHandle {
    let completion_handler =
        SwiftCompletionHandler::new(unsafe { CompletionCookie::new(completion_cookie) });

    let Ok(tokio_handle) = crate::mullvad_ios_runtime() else {
        completion_handler.finish(SwiftMullvadApiResponse::no_tokio_runtime());
        return SwiftCancelHandle::empty();
    };

    let api_context = api_context.rust_context();

    // SAFETY: See SwiftRetryStrategy::into_rust.
    let retry_strategy = unsafe { retry_strategy.into_rust() };

    let completion = completion_handler.clone();

    // SAFETY: See param documentation for `account_number`.
    let account_number = AccountNumber::from(unsafe { get_string(account_number) });

    let task = tokio_handle.spawn(async move {
        match mullvad_ios_init_storekit_payment_inner(
            api_context.rest_handle(),
            retry_strategy,
            account_number,
        )
        .await
        {
            Ok(response) => completion.finish(response),
            Err(err) => {
                log::error!("{err:?}");
                completion.finish(SwiftMullvadApiResponse::rest_error(err));
            }
        }
    });

    RequestCancelHandle::new(task, completion_handler.clone()).into_swift()
}

async fn mullvad_ios_init_storekit_payment_inner(
    rest_client: MullvadRestHandle,
    retry_strategy: RetryStrategy,
    account_number: AccountNumber,
) -> Result<SwiftMullvadApiResponse, rest::Error> {
    let account_proxy = AccountsProxy::new(rest_client);

    let future_factory = || account_proxy.init_storekit_payment(account_number.clone());

    do_request(retry_strategy, future_factory).await
}

/// # Safety
///
/// `api_context` must be pointing to a valid instance of `SwiftApiContext`. A `SwiftApiContext` is created
/// by calling `mullvad_api_init_new`.
///
/// This function takes ownership of `completion_cookie`, which must be pointing to a valid instance of Swift
/// object `MullvadApiCompletion`. The pointer will be freed by calling `mullvad_api_completion_finish`
/// when completion finishes (in completion.finish).
///
/// `retry_strategy` must have been created by a call to either of the following functions
/// `mullvad_api_retry_strategy_never`, `mullvad_api_retry_strategy_constant` or `mullvad_api_retry_strategy_exponential`
///
/// `account_number` must be a pointer to a null terminated string.
///
/// `body` must be a pointer to a contiguous memory segment
///
/// `body_size` must be the size of the body
///
/// This function is not safe to call multiple times with the same `CompletionCookie`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mullvad_ios_check_storekit_payment(
    api_context: SwiftApiContext,
    completion_cookie: *mut libc::c_void,
    retry_strategy: SwiftRetryStrategy,
    account_number: *const c_char,
    body: *const u8,
    body_size: usize,
) -> SwiftCancelHandle {
    let completion_handler =
        SwiftCompletionHandler::new(unsafe { CompletionCookie::new(completion_cookie) });

    let Ok(tokio_handle) = crate::mullvad_ios_runtime() else {
        completion_handler.finish(SwiftMullvadApiResponse::no_tokio_runtime());
        return SwiftCancelHandle::empty();
    };

    let api_context = api_context.rust_context();
    // SAFETY: See SwiftRetryStrategy::into_rust.
    let retry_strategy = unsafe { retry_strategy.into_rust() };

    let completion = completion_handler.clone();

    // SAFETY: See param documentation for `account_number`.
    let account_number = AccountNumber::from(unsafe { get_string(account_number) });

    // SAFETY: See param documentation for `body`.
    let body = unsafe { std::slice::from_raw_parts(body, body_size) }.to_vec();
    let task = tokio_handle.spawn(async move {
        match mullvad_ios_check_storekit_payment_inner(
            api_context.rest_handle(),
            retry_strategy,
            account_number,
            body,
        )
        .await
        {
            Ok(response) => completion.finish(response),
            Err(err) => {
                log::error!("{err:?}");
                completion.finish(SwiftMullvadApiResponse::rest_error(err));
            }
        }
    });

    RequestCancelHandle::new(task, completion_handler.clone()).into_swift()
}

async fn mullvad_ios_check_storekit_payment_inner(
    rest_client: MullvadRestHandle,
    retry_strategy: RetryStrategy,
    account_number: AccountNumber,
    body: Vec<u8>,
) -> Result<SwiftMullvadApiResponse, rest::Error> {
    let account_proxy = AccountsProxy::new(rest_client);

    let future_factory =
        || account_proxy.check_storekit_payment(account_number.clone(), body.clone());

    do_request(retry_strategy, future_factory).await
}
