use chrono::{DateTime, Utc};
use futures::{
    channel::{mpsc, oneshot},
    stream::StreamExt,
};

use mullvad_api::rest;
#[cfg(target_os = "android")]
use mullvad_types::account::{PlayPurchase, PlayPurchasePaymentToken};
use mullvad_types::{
    account::{AccountNumber, VoucherSubmission},
    device::{
        AccountAndDevice, Device, DeviceEvent, DeviceEventCause, DeviceId, DeviceName, DeviceState,
    },
    wireguard::{self, RotationInterval, WireguardData},
};

use std::{
    future::Future,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};
use talpid_core::mpsc::Sender;
use talpid_types::{
    ErrorExt,
    net::{TunnelEndpoint, TunnelType},
    tunnel::TunnelStateTransition,
};
use tokio::{
    fs,
    io::{self, AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
};

mod api;
mod service;
pub(crate) use service::{AccountService, DeviceService};

/// File that used to store account and device data.
const DEVICE_CACHE_FILENAME: &str = "device.json";

/// How long to keep the known status for [AccountManagerHandle::validate_device].
const VALIDITY_CACHE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait on logout (device removal) before letting it continue as a background task.
const LOGOUT_TIMEOUT: Duration = Duration::from_secs(2);

/// Validate the current device once for every `WG_DEVICE_CHECK_THRESHOLD` attempt to set up
/// a WireGuard tunnel.
const WG_DEVICE_CHECK_THRESHOLD: usize = 3;

#[derive(thiserror::Error, Debug, Clone)]
pub enum Error {
    #[error("The account already has a maximum number of devices")]
    MaxDevicesReached,
    #[error("No device is set")]
    NoDevice,
    #[error("Device not found")]
    InvalidDevice,
    #[error("Invalid account")]
    InvalidAccount,
    #[error("Invalid voucher code")]
    InvalidVoucher,
    #[error("The voucher has already been used")]
    UsedVoucher,
    #[error("Failed to read or write device cache")]
    DeviceIoError(#[from] Arc<io::Error>),
    #[error("Failed parse device cache")]
    ParseDeviceCache(#[from] Arc<serde_json::Error>),
    #[error("Unexpected HTTP request error")]
    OtherRestError(#[from] rest::Error),
    #[error("The device update task is not running")]
    Cancelled,
    #[error("Account changed during operation")]
    AccountChange,
    #[error("The account manager is down")]
    AccountManagerDown,
}

macro_rules! impl_into_arc_err {
    ($ty:ty) => {
        impl From<$ty> for Error {
            fn from(error: $ty) -> Self {
                Error::from(Arc::from(error))
            }
        }
    };
}

impl_into_arc_err!(io::Error);
impl_into_arc_err!(serde_json::Error);

/// Contains the current device state.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrivateDeviceState {
    LoggedIn(PrivateAccountAndDevice),
    LoggedOut,
    Revoked,
}

impl PrivateDeviceState {
    /// Returns whether the device is in the logged in state.
    pub fn logged_in(&self) -> bool {
        matches!(self, PrivateDeviceState::LoggedIn(_))
    }

    /// Returns whether the state is logged out, as opposed to
    /// logged in or revoked.
    pub fn logged_out(&self) -> bool {
        matches!(self, PrivateDeviceState::LoggedOut)
    }

    /// Returns the logged in device config.
    pub fn device(&self) -> Option<&PrivateAccountAndDevice> {
        match self {
            PrivateDeviceState::LoggedIn(device) => Some(device),
            _ => None,
        }
    }

    /// Returns the logged in device config.
    pub fn into_device(self) -> Option<PrivateAccountAndDevice> {
        match self {
            PrivateDeviceState::LoggedIn(device) => Some(device),
            _ => None,
        }
    }

    /// Sets the state to `Revoked`.
    fn revoke(&mut self) {
        *self = PrivateDeviceState::Revoked;
    }

    /// Sets the state to `LoggedOut` and returns the logged-in device, if one exists.
    fn logout(&mut self) -> Option<PrivateAccountAndDevice> {
        match std::mem::replace(self, PrivateDeviceState::LoggedOut) {
            PrivateDeviceState::LoggedIn(data) => Some(data),
            _ => None,
        }
    }
}

impl From<PrivateDeviceState> for DeviceState {
    fn from(state: PrivateDeviceState) -> Self {
        match state {
            PrivateDeviceState::LoggedIn(dev) => DeviceState::LoggedIn(AccountAndDevice::from(dev)),
            PrivateDeviceState::LoggedOut => DeviceState::LoggedOut,
            PrivateDeviceState::Revoked => DeviceState::Revoked,
        }
    }
}

/// Same as [PrivateDevice] but also contains the associated account number.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct PrivateAccountAndDevice {
    #[serde(alias = "account_token")]
    pub account_number: AccountNumber,
    pub device: PrivateDevice,
}

impl From<PrivateAccountAndDevice> for AccountAndDevice {
    fn from(config: PrivateAccountAndDevice) -> Self {
        AccountAndDevice {
            account_number: config.account_number,
            device: Device::from(config.device),
        }
    }
}

/// Device type that contains private data.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct PrivateDevice {
    pub id: DeviceId,
    pub name: DeviceName,
    pub wg_data: wireguard::WireguardData,
    // FIXME: Reasonable default to avoid migration code for the field,
    // as it was previously missing.
    // This attribute may be removed once upgrades from `2022.2-beta1`
    // no longer need to be supported.
    #[serde(default)]
    pub hijack_dns: bool,
    // FIXME: Incorrect but reasonable default to avoid migration code
    // for the field, as it was previously missing.
    // The value is corrected when the device is validated or updated.
    // This attribute may be removed once upgrades from `2022.2-beta1`
    // no longer need to be supported.
    #[serde(default = "Utc::now")]
    pub created: DateTime<Utc>,
}

impl PrivateDevice {
    /// Construct a private device from a `WireguardData` and a `Device`. Fails if the pubkey of
    /// `device` does not match that of `wg_data`.
    pub fn try_from_device(
        device: Device,
        wg_data: wireguard::WireguardData,
    ) -> Result<Self, Error> {
        if device.pubkey != wg_data.private_key.public_key() {
            return Err(Error::InvalidDevice);
        }
        Ok(Self {
            id: device.id,
            name: device.name,
            wg_data,
            hijack_dns: device.hijack_dns,
            created: device.created,
        })
    }

    /// Update all device details that are present in both types. Fails if the pubkey of `device`
    /// does not match that of `wg_data`.
    fn update(&mut self, device: Device) -> Result<(), Error> {
        if device.pubkey != self.wg_data.private_key.public_key() {
            return Err(Error::InvalidDevice);
        }
        self.id = device.id;
        self.name = device.name;
        self.hijack_dns = device.hijack_dns;
        self.created = device.created;
        Ok(())
    }
}

impl From<PrivateDevice> for Device {
    fn from(device: PrivateDevice) -> Self {
        Device {
            id: device.id,
            pubkey: device.wg_data.private_key.public_key(),
            name: device.name,
            hijack_dns: device.hijack_dns,
            created: device.created,
        }
    }
}

#[derive(Clone)]
pub(crate) enum AccountEvent {
    /// Emitted when the device state changes.
    Device(PrivateDeviceEvent),
    /// Emitted when the account expiry is fetched.
    Expiry(DateTime<Utc>),
}

#[derive(Clone)]
pub(crate) enum PrivateDeviceEvent {
    /// Logged in on a new device.
    Login(PrivateAccountAndDevice),
    /// The device was removed due to user (or daemon) action.
    Logout,
    /// Device was removed because it was not found remotely.
    Revoked,
    /// The device was updated remotely, but not its key.
    Updated(PrivateAccountAndDevice),
    /// The key was rotated.
    RotatedKey(PrivateAccountAndDevice),
}

impl From<PrivateDeviceEvent> for DeviceEvent {
    fn from(event: PrivateDeviceEvent) -> DeviceEvent {
        let cause = match event {
            PrivateDeviceEvent::Login(_) => DeviceEventCause::LoggedIn,
            PrivateDeviceEvent::Logout => DeviceEventCause::LoggedOut,
            PrivateDeviceEvent::Revoked => DeviceEventCause::Revoked,
            PrivateDeviceEvent::Updated(_) => DeviceEventCause::Updated,
            PrivateDeviceEvent::RotatedKey(_) => DeviceEventCause::RotatedKey,
        };
        let new_state = DeviceState::from(event.state());
        DeviceEvent { cause, new_state }
    }
}

impl PrivateDeviceEvent {
    pub fn state(self) -> PrivateDeviceState {
        match self {
            PrivateDeviceEvent::Login(config) => PrivateDeviceState::LoggedIn(config),
            PrivateDeviceEvent::Updated(config) => PrivateDeviceState::LoggedIn(config),
            PrivateDeviceEvent::RotatedKey(config) => PrivateDeviceState::LoggedIn(config),
            PrivateDeviceEvent::Logout => PrivateDeviceState::LoggedOut,
            PrivateDeviceEvent::Revoked => PrivateDeviceState::Revoked,
        }
    }
}

impl Error {
    pub fn is_network_error(&self) -> bool {
        matches!(self, Error::OtherRestError(error) if error.is_network_error())
    }

    pub fn is_aborted(&self) -> bool {
        matches!(self, Error::OtherRestError(error) if error.is_aborted())
    }
}

type ResponseTx<T> = oneshot::Sender<Result<T, Error>>;

enum AccountManagerCommand {
    Login(AccountNumber, ResponseTx<()>),
    Logout(ResponseTx<()>),
    SetData(PrivateAccountAndDevice, ResponseTx<()>),
    GetData(ResponseTx<PrivateDeviceState>),
    GetDataAfterLogin(ResponseTx<PrivateDeviceState>),
    RotateKey(ResponseTx<()>),
    SetRotationInterval(RotationInterval, ResponseTx<()>),
    ValidateDevice(ResponseTx<()>),
    SubmitVoucher(String, ResponseTx<VoucherSubmission>),
    #[cfg(target_os = "android")]
    InitPlayPurchase(ResponseTx<PlayPurchasePaymentToken>),
    #[cfg(target_os = "android")]
    VerifyPlayPurchase(ResponseTx<()>, PlayPurchase),
    CheckExpiry(ResponseTx<DateTime<Utc>>),
    Shutdown(oneshot::Sender<()>),
}

#[derive(Clone)]
pub(crate) struct AccountManagerHandle {
    cmd_tx: mpsc::UnboundedSender<AccountManagerCommand>,
    pub account_service: AccountService,
    pub device_service: DeviceService,
}

impl AccountManagerHandle {
    pub async fn login(&self, number: AccountNumber) -> Result<(), Error> {
        self.send_command(|tx| AccountManagerCommand::Login(number, tx))
            .await
    }

    pub async fn logout(&self) -> Result<(), Error> {
        self.send_command(AccountManagerCommand::Logout).await
    }

    pub async fn set(&self, data: PrivateAccountAndDevice) -> Result<(), Error> {
        self.send_command(|tx| AccountManagerCommand::SetData(data, tx))
            .await
    }

    pub async fn data(&self) -> Result<PrivateDeviceState, Error> {
        self.send_command(AccountManagerCommand::GetData).await
    }

    pub async fn data_after_login(&self) -> Result<PrivateDeviceState, Error> {
        self.send_command(AccountManagerCommand::GetDataAfterLogin)
            .await
    }

    pub async fn rotate_key(&self) -> Result<(), Error> {
        self.send_command(AccountManagerCommand::RotateKey).await
    }

    pub async fn set_rotation_interval(&self, interval: RotationInterval) -> Result<(), Error> {
        self.send_command(|tx| AccountManagerCommand::SetRotationInterval(interval, tx))
            .await
    }

    pub async fn validate_device(&self) -> Result<(), Error> {
        self.send_command(AccountManagerCommand::ValidateDevice)
            .await
    }

    pub async fn submit_voucher(&self, voucher: String) -> Result<VoucherSubmission, Error> {
        self.send_command(move |tx| AccountManagerCommand::SubmitVoucher(voucher, tx))
            .await
    }

    pub async fn check_expiry(&self) -> Result<DateTime<Utc>, Error> {
        self.send_command(AccountManagerCommand::CheckExpiry).await
    }

    #[cfg(target_os = "android")]
    pub async fn init_play_purchase(&self) -> Result<PlayPurchasePaymentToken, Error> {
        self.send_command(AccountManagerCommand::InitPlayPurchase)
            .await
    }

    #[cfg(target_os = "android")]
    pub async fn verify_play_purchase(&self, play_purchase: PlayPurchase) -> Result<(), Error> {
        self.send_command(move |tx| AccountManagerCommand::VerifyPlayPurchase(tx, play_purchase))
            .await
    }

    pub async fn shutdown(self) {
        let (tx, rx) = oneshot::channel();
        let _ = self
            .cmd_tx
            .unbounded_send(AccountManagerCommand::Shutdown(tx));
        let _ = rx.await;
    }

    async fn send_command<T>(
        &self,
        make_cmd: impl FnOnce(oneshot::Sender<Result<T, Error>>) -> AccountManagerCommand,
    ) -> Result<T, Error> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .unbounded_send(make_cmd(tx))
            .map_err(|_| Error::AccountManagerDown)?;
        rx.await.map_err(|_| Error::AccountManagerDown)?
    }
}

pub(crate) struct AccountManager {
    cacher: DeviceCacher,
    account_service: AccountService,
    device_service: DeviceService,
    data: PrivateDeviceState,
    rotation_interval: RotationInterval,
    listeners: Vec<Box<dyn Sender<AccountEvent> + Send>>,
    last_validation: Option<SystemTime>,
    validation_requests: Vec<ResponseTx<()>>,
    expiry_requests: Vec<ResponseTx<DateTime<Utc>>>,
    rotation_requests: Vec<ResponseTx<()>>,
    data_requests: Vec<ResponseTx<PrivateDeviceState>>,
}

impl AccountManager {
    /// Starts the account manager actor and returns a handle to it as well as the
    /// current device.
    pub async fn spawn(
        rest_handle: rest::MullvadRestHandle,
        settings_dir: &Path,
        initial_rotation_interval: RotationInterval,
        listener_tx: impl Sender<AccountEvent> + Send + 'static,
    ) -> Result<(AccountManagerHandle, PrivateDeviceState), Error> {
        let (cacher, data) = DeviceCacher::new(settings_dir).await?;
        let number = data.device().map(|state| state.account_number.clone());
        let api_availability = rest_handle.availability.clone();
        let account_service =
            service::spawn_account_service(rest_handle.clone(), number, api_availability.clone());

        let (cmd_tx, cmd_rx) = mpsc::unbounded();

        let device_service = DeviceService::new(rest_handle, api_availability);
        let manager = AccountManager {
            cacher,
            account_service: account_service.clone(),
            device_service: device_service.clone(),
            data: data.clone(),
            rotation_interval: initial_rotation_interval,
            listeners: vec![Box::new(listener_tx)],
            last_validation: None,
            validation_requests: vec![],
            expiry_requests: vec![],
            rotation_requests: vec![],
            data_requests: vec![],
        };

        tokio::spawn(manager.run(cmd_rx));
        let handle = AccountManagerHandle {
            cmd_tx,
            account_service,
            device_service,
        };
        Ok((handle, data))
    }

    async fn run(mut self, mut cmd_rx: mpsc::UnboundedReceiver<AccountManagerCommand>) {
        let mut shutdown_tx = None;
        let mut current_api_call = api::CurrentApiCall::new();

        loop {
            if current_api_call.is_idle()
                && let Some(timed_rotation) = self.spawn_timed_key_rotation()
            {
                current_api_call.set_timed_rotation(Box::pin(timed_rotation))
            }

            futures::select! {
                api_result = current_api_call => {
                    self.consume_api_result(api_result, &mut current_api_call).await;
                }

                cmd = cmd_rx.next() => {
                    match cmd {
                        Some(AccountManagerCommand::Shutdown(tx)) => {
                            shutdown_tx = Some(tx);
                            break;
                        }
                        Some(AccountManagerCommand::Login(number, tx)) => {
                            let job = self.device_service
                                .generate_for_account(number);
                            current_api_call.set_login(Box::pin(job), tx);
                        }
                        Some(AccountManagerCommand::Logout(tx)) => {
                            current_api_call.clear();
                            self.logout(tx).await;
                        }
                        Some(AccountManagerCommand::SetData(data, tx)) => {
                            let _ = tx.send(self.set(PrivateDeviceEvent::Login(data)).await);
                        }
                        Some(AccountManagerCommand::GetData(tx)) => {
                            let _ = tx.send(Ok(self.data.clone()));
                        }
                        Some(AccountManagerCommand::GetDataAfterLogin(tx)) => {
                            if current_api_call.is_logging_in() {
                                self.data_requests.push(tx);
                            } else {
                                let _ = tx.send(Ok(self.data.clone()));
                            }
                        }
                        Some(AccountManagerCommand::RotateKey(tx)) => {
                            if current_api_call.is_logging_in() {
                                let _ = tx.send(Err(Error::AccountChange));
                                continue
                            }
                            if current_api_call.is_validating() {
                                self.rotation_requests.push(tx);
                                continue
                            }
                            match self.initiate_key_rotation() {
                                Ok(api_call) => {
                                    current_api_call.set_oneshot_rotation(Box::pin(api_call));
                                    self.rotation_requests.push(tx);
                                },
                                Err(err) =>  {
                                    let _ = tx.send(Err(err));
                                }
                            }
                        }
                        Some(AccountManagerCommand::SetRotationInterval(interval, tx)) => {
                            self.rotation_interval = interval;
                            if current_api_call.is_running_timed_totation() {
                                current_api_call.clear();
                            }
                            let _ = tx.send(Ok(()));
                        }
                        Some(AccountManagerCommand::ValidateDevice(tx)) => {
                            self.handle_validation_request(tx, &mut current_api_call);
                        },
                        Some(AccountManagerCommand::SubmitVoucher(voucher, tx)) => {
                            self.handle_voucher_submission(tx, voucher, &mut current_api_call);
                        },
                        Some(AccountManagerCommand::CheckExpiry(tx)) => {
                            self.handle_expiry_request(tx, &mut current_api_call);
                        },
                        #[cfg(target_os = "android")]
                        Some(AccountManagerCommand::InitPlayPurchase(tx)) => {
                            self.handle_init_play_purchase(tx, &mut current_api_call);
                        },
                        #[cfg(target_os = "android")]
                        Some(AccountManagerCommand::VerifyPlayPurchase(tx, play_purchase)) => {
                            self.handle_verify_play_purchase(tx, play_purchase, &mut current_api_call);
                        },

                        None => {
                            break;
                        }
                    }
                }
            }
        }
        self.shutdown().await;
        if let Some(tx) = shutdown_tx {
            let _ = tx.send(());
        }
        log::debug!("Account manager has stopped");
    }

    fn handle_validation_request(
        &mut self,
        tx: ResponseTx<()>,
        current_api_call: &mut api::CurrentApiCall,
    ) {
        if current_api_call.is_logging_in() {
            let _ = tx.send(Err(Error::AccountChange));
            return;
        }
        if current_api_call.is_validating() {
            self.validation_requests.push(tx);
            return;
        }
        if !self.needs_validation() {
            let _ = tx.send(Ok(()));
            return;
        }

        match self.validation_call() {
            Ok(call) => {
                current_api_call.set_validation(Box::pin(call));
                self.validation_requests.push(tx);
            }
            Err(err) => {
                let _ = tx.send(Err(err));
            }
        }
    }

    fn handle_voucher_submission(
        &mut self,
        tx: ResponseTx<VoucherSubmission>,
        voucher: String,
        current_api_call: &mut api::CurrentApiCall,
    ) {
        if current_api_call.is_logging_in() {
            let _ = tx.send(Err(Error::AccountChange));
            return;
        }

        let create_submission = move || {
            let old_config = self.data.device().ok_or(Error::NoDevice)?;
            let account_number = old_config.account_number.clone();
            let account_service = self.account_service.clone();
            Ok(async move {
                account_service
                    .submit_voucher(account_number, voucher)
                    .await
            })
        };

        match create_submission() {
            Ok(call) => {
                current_api_call.set_voucher_submission(Box::pin(call), tx);
            }
            Err(err) => {
                let _ = tx.send(Err(err));
            }
        }
    }

    #[cfg(target_os = "android")]
    fn handle_init_play_purchase(
        &mut self,
        tx: ResponseTx<PlayPurchasePaymentToken>,
        current_api_call: &mut api::CurrentApiCall,
    ) {
        if current_api_call.is_logging_in() {
            let _ = tx.send(Err(Error::AccountChange));
            return;
        }

        let init_play_purchase_api_call = move || {
            let old_config = self.data.device().ok_or(Error::NoDevice)?;
            let account_number = old_config.account_number.clone();
            let account_service = self.account_service.clone();
            Ok(async move { account_service.init_play_purchase(account_number).await })
        };

        match init_play_purchase_api_call() {
            Ok(call) => {
                current_api_call.set_init_play_purchase(Box::pin(call), tx);
            }
            Err(err) => {
                let _ = tx.send(Err(err));
            }
        }
    }

    fn handle_expiry_request(
        &mut self,
        tx: ResponseTx<DateTime<Utc>>,
        current_api_call: &mut api::CurrentApiCall,
    ) {
        if current_api_call.is_logging_in() {
            let _ = tx.send(Err(Error::AccountChange));
            return;
        }
        if current_api_call.is_checking_expiry() {
            self.expiry_requests.push(tx);
            return;
        }

        match self.expiry_call() {
            Ok(call) => {
                current_api_call.set_expiry_check(Box::pin(call));
                self.expiry_requests.push(tx);
            }
            Err(err) => {
                let _ = tx.send(Err(err));
            }
        }
    }

    #[cfg(target_os = "android")]
    fn handle_verify_play_purchase(
        &mut self,
        tx: ResponseTx<()>,
        play_purchase: PlayPurchase,
        current_api_call: &mut api::CurrentApiCall,
    ) {
        if current_api_call.is_logging_in() {
            let _ = tx.send(Err(Error::AccountChange));
            return;
        }

        let play_purchase_verify_api_call = move || {
            let old_config = self.data.device().ok_or(Error::NoDevice)?;
            let account_number = old_config.account_number.clone();
            let account_service = self.account_service.clone();
            Ok(async move {
                account_service
                    .verify_play_purchase(account_number, play_purchase)
                    .await
            })
        };

        match play_purchase_verify_api_call() {
            Ok(call) => {
                current_api_call.set_verify_play_purchase(Box::pin(call), tx);
            }
            Err(err) => {
                let _ = tx.send(Err(err));
            }
        }
    }

    async fn consume_api_result(
        &mut self,
        result: api::ApiResult,
        api_call: &mut api::CurrentApiCall,
    ) {
        use api::ApiResult::*;
        match result {
            Login(data, tx) => self.consume_login(data, tx).await,
            Rotation(rotation_response) => self.consume_rotation_result(rotation_response).await,
            Validation(data_response) => self.consume_validation(data_response, api_call).await,
            VoucherSubmission(data_response, tx) => {
                self.consume_voucher_result(data_response, tx).await
            }
            ExpiryCheck(data_response) => self.consume_expiry_result(data_response).await,
            #[cfg(target_os = "android")]
            InitPlayPurchase(data_response, tx) => {
                self.consume_init_play_purchase_result(data_response, tx)
                    .await
            }
            #[cfg(target_os = "android")]
            VerifyPlayPurchase(data_response, tx) => {
                self.consume_verify_play_purchase_result(data_response, tx)
                    .await
            }
        }
    }

    async fn consume_login(
        &mut self,
        device_response: Result<PrivateAccountAndDevice, Error>,
        tx: ResponseTx<()>,
    ) {
        let _ =
            tx.send(async { self.set(PrivateDeviceEvent::Login(device_response?)).await }.await);
        let data = self.data.clone();
        Self::drain_requests(&mut self.data_requests, || Ok(data.clone()));
    }

    async fn consume_voucher_result(
        &mut self,
        response: Result<VoucherSubmission, Error>,
        tx: ResponseTx<VoucherSubmission>,
    ) {
        match &response {
            Ok(submission) => {
                // Send expiry update event
                let event = AccountEvent::Expiry(submission.new_expiry);
                self.listeners
                    .retain(|listener| listener.send(event.clone()).is_ok());
            }
            Err(Error::InvalidAccount) => {
                self.revoke_device(|| Error::InvalidAccount).await;
            }
            Err(Error::InvalidDevice) => {
                self.revoke_device(|| Error::InvalidDevice).await;
            }
            Err(err) => log::error!("Failed to submit voucher: {}", err),
        }
        let _ = tx.send(response);
    }

    async fn consume_expiry_result(&mut self, response: Result<DateTime<Utc>, Error>) {
        match response {
            Ok(expiry) => {
                if expiry > chrono::Utc::now() {
                    log::debug!("Account has time left");
                } else {
                    log::debug!("Account has no time left");
                }

                // Send expiry update event
                let event = AccountEvent::Expiry(expiry);
                self.listeners
                    .retain(|listener| listener.send(event.clone()).is_ok());

                Self::drain_requests(&mut self.expiry_requests, || Ok(expiry));
            }
            Err(Error::InvalidAccount) => {
                self.revoke_device(|| Error::InvalidAccount).await;
            }
            Err(Error::InvalidDevice) => {
                self.revoke_device(|| Error::InvalidDevice).await;
            }
            Err(err) => {
                log::error!("Failed to check account expiry: {}", err);
                Self::drain_requests(&mut self.expiry_requests, || Err(err.clone()));
            }
        }
    }

    async fn consume_validation(
        &mut self,
        response: Result<Device, Error>,
        api_call: &mut api::CurrentApiCall,
    ) {
        let current_config = self
            .data
            .device()
            .expect("Received a validation response whilst having no device data");

        match response {
            Ok(new_device) => {
                let current_pubkey = current_config.device.wg_data.private_key.public_key();
                if new_device.pubkey == current_pubkey {
                    let mut new_data = current_config.clone();
                    new_data
                        .device
                        .update(new_device)
                        .expect("pubkey must match privkey");

                    if Some(&new_data) != self.data.device() {
                        log::debug!("Updating data for the current device");
                    } else {
                        log::debug!("The current device is still valid");
                    }

                    match self.set(PrivateDeviceEvent::Updated(new_data)).await {
                        Ok(_) => {
                            Self::drain_requests(&mut self.validation_requests, || Ok(()));
                        }
                        Err(err) => {
                            log::error!("Failed to save device data to disk");
                            Self::drain_requests(
                                &mut self.validation_requests,
                                || Err(err.clone()),
                            );
                        }
                    }
                } else {
                    log::debug!("Rotating invalid WireGuard key for device");
                }
            }
            Err(Error::InvalidAccount) => {
                self.revoke_device(|| Error::InvalidAccount).await;
            }
            Err(Error::InvalidDevice) => {
                self.revoke_device(|| Error::InvalidDevice).await;
            }
            Err(err) => {
                log::error!("Failed to validate device: {}", err);
                Self::drain_requests(&mut self.validation_requests, || Err(err.clone()));
            }
        }

        if (!self.rotation_requests.is_empty() || !self.validation_requests.is_empty())
            && let Some(updated_config) = self.data.device()
        {
            let device_service = self.device_service.clone();
            let number = updated_config.account_number.clone();
            let device_id = updated_config.device.id.clone();
            api_call.set_oneshot_rotation(Box::pin(async move {
                device_service.rotate_key(number, device_id).await
            }));
        }
    }

    async fn consume_rotation_result(&mut self, api_result: Result<WireguardData, Error>) {
        let mut config = self
            .data
            .device()
            .cloned()
            .expect("Received a key rotation result whilst having no data");

        match api_result {
            Ok(wg_data) => {
                log::debug!("Replacing WireGuard key");
                config.device.wg_data = wg_data;
                match self.set(PrivateDeviceEvent::RotatedKey(config)).await {
                    Ok(_) => {
                        Self::drain_requests(&mut self.rotation_requests, || Ok(()));
                        Self::drain_requests(&mut self.validation_requests, || Ok(()));
                    }
                    Err(err) => {
                        self.drain_device_requests_with_err(err);
                    }
                }
            }
            Err(Error::InvalidAccount) => {
                self.revoke_device(|| Error::InvalidAccount).await;
            }
            Err(Error::InvalidDevice) => {
                self.revoke_device(|| Error::InvalidDevice).await;
            }
            Err(err) => {
                self.drain_device_requests_with_err(err);
            }
        }
    }

    #[cfg(target_os = "android")]
    async fn consume_init_play_purchase_result(
        &mut self,
        response: Result<PlayPurchasePaymentToken, Error>,
        tx: ResponseTx<PlayPurchasePaymentToken>,
    ) {
        match &response {
            Ok(_) => (),
            Err(Error::InvalidAccount) => {
                self.revoke_device(|| Error::InvalidAccount).await;
            }
            Err(Error::InvalidDevice) => {
                self.revoke_device(|| Error::InvalidDevice).await;
            }
            Err(err) => log::error!("Failed to initialize play purchase: {}", err),
        }
        let _ = tx.send(response);
    }

    #[cfg(target_os = "android")]
    async fn consume_verify_play_purchase_result(
        &mut self,
        response: Result<(), Error>,
        tx: ResponseTx<()>,
    ) {
        match &response {
            Ok(_) => (),
            Err(Error::InvalidAccount) => {
                self.revoke_device(|| Error::InvalidAccount).await;
            }
            Err(Error::InvalidDevice) => {
                self.revoke_device(|| Error::InvalidDevice).await;
            }
            Err(err) => log::error!("Failed to verify play purchase: {}", err),
        }
        let _ = tx.send(response);
    }

    fn drain_device_requests_with_err(&mut self, err: Error) {
        Self::drain_requests(&mut self.rotation_requests, || Err(err.clone()));
        Self::drain_requests(&mut self.validation_requests, || Err(err.clone()));
    }

    fn drain_requests<T>(requests: &mut Vec<ResponseTx<T>>, result: impl Fn() -> Result<T, Error>) {
        for req in requests.drain(0..) {
            let _ = req.send(result());
        }
    }

    fn spawn_timed_key_rotation(
        &self,
    ) -> Option<impl Future<Output = Result<WireguardData, Error>> + Send + 'static + use<>> {
        let config = self.data.device()?;
        let key_rotation_timer = self.key_rotation_timer(config.device.wg_data.created);

        let device_service = self.device_service.clone();
        let account_number = config.account_number.clone();
        let device_id = config.device.id.clone();

        Some(async move {
            key_rotation_timer.await;
            device_service
                .rotate_key_with_backoff(account_number, device_id)
                .await
        })
    }

    async fn revoke_device(&mut self, err_constructor: impl Fn() -> Error) {
        log::debug!("Invalidating the current device");

        if let Err(err) = self.cacher.write(&PrivateDeviceState::Revoked).await {
            log::error!(
                "{}",
                err.display_chain_with_msg("Failed to save device data to disk")
            );
        }
        self.data.revoke();

        Self::drain_requests(&mut self.validation_requests, || Err(err_constructor()));
        Self::drain_requests(&mut self.rotation_requests, || Err(err_constructor()));
        Self::drain_requests(&mut self.expiry_requests, || Err(err_constructor()));

        self.listeners.retain(|listener| {
            listener
                .send(AccountEvent::Device(PrivateDeviceEvent::Revoked))
                .is_ok()
        });
    }

    async fn logout(&mut self, tx: ResponseTx<()>) {
        Self::drain_requests(&mut self.data_requests, || Err(Error::AccountChange));
        if self.data.logged_out() {
            let _ = tx.send(Ok(()));
            return;
        }
        if let Err(err) = self.cacher.write(&PrivateDeviceState::LoggedOut).await {
            let _ = tx.send(Err(err));
            return;
        }

        let old_config = self.data.logout();

        self.listeners.retain(|listener| {
            listener
                .send(AccountEvent::Device(PrivateDeviceEvent::Logout))
                .is_ok()
        });

        match old_config {
            Some(old_config) => {
                let logout_call = tokio::spawn(Box::pin(self.logout_api_call(old_config)));

                tokio::spawn(async move {
                    let _response = tokio::time::timeout(LOGOUT_TIMEOUT, logout_call).await;
                    let _ = tx.send(Ok(()));
                });
            }
            _ => {
                // The state was `revoked`.
                let _ = tx.send(Ok(()));
            }
        }
    }

    fn logout_api_call(
        &self,
        data: PrivateAccountAndDevice,
    ) -> impl Future<Output = ()> + 'static + use<> {
        let service = self.device_service.clone();

        async move {
            if let Err(error) = service
                .remove_device_with_backoff(data.account_number, data.device.id)
                .await
            {
                log::error!(
                    "{}",
                    error.display_chain_with_msg("Failed to logout device")
                );
            }
        }
    }

    async fn set(&mut self, event: PrivateDeviceEvent) -> Result<(), Error> {
        let device_state = event.clone().state();
        if device_state == self.data {
            return Ok(());
        }

        self.cacher.write(&device_state).await?;
        self.last_validation = None;

        if let Some(old_config) = self.data.logout()
            && device_state.device().map(|d| &d.device.id) != Some(&old_config.device.id)
        {
            tokio::spawn(self.logout_api_call(old_config));
        }

        self.data = device_state;

        let event = AccountEvent::Device(event);
        self.listeners
            .retain(|listener| listener.send(event.clone()).is_ok());

        Ok(())
    }

    fn initiate_key_rotation(
        &self,
    ) -> Result<impl Future<Output = Result<WireguardData, Error>> + use<>, Error> {
        let data = self.data.device().cloned().ok_or(Error::NoDevice)?;
        let device_service = self.device_service.clone();
        Ok(async move {
            device_service
                .rotate_key(data.account_number, data.device.id)
                .await
        })
    }

    fn key_rotation_timer(
        &self,
        key_created: DateTime<Utc>,
    ) -> impl Future<Output = ()> + 'static + use<> {
        let rotation_interval = self.rotation_interval;

        async move {
            let key_age = Duration::from_secs(
                chrono::Utc::now()
                        .signed_duration_since(key_created)
                        .num_seconds()
                        .try_into()
                        // This would only fail if the key was created in the future, in which case
                        // the duration would be negative. In this case, I think it's safe to
                        // assume the daemon should wait one whole key rotation interval.
                        .unwrap_or(0u64),
            );
            let time_until_next_rotation = std::cmp::max(
                rotation_interval.as_duration().saturating_sub(key_age),
                Duration::from_secs(60),
            );

            log::trace!(
                "{} seconds to wait until next rotation",
                time_until_next_rotation.as_secs(),
            );
            talpid_time::sleep(time_until_next_rotation).await
        }
    }

    fn fetch_device_config(
        &self,
        old_config: &PrivateAccountAndDevice,
    ) -> impl Future<Output = Result<Device, Error>> + use<> {
        let device_service = self.device_service.clone();
        let account_number = old_config.account_number.clone();
        let device_id = old_config.device.id.clone();
        async move { device_service.get(account_number, device_id).await }
    }

    fn validation_call(
        &self,
    ) -> Result<impl Future<Output = Result<Device, Error>> + use<>, Error> {
        let old_config = self.data.device().ok_or(Error::NoDevice)?;
        Ok(self.fetch_device_config(old_config))
    }

    fn expiry_call(
        &self,
    ) -> Result<impl Future<Output = Result<chrono::DateTime<Utc>, Error>> + use<>, Error> {
        let old_config = self.data.device().ok_or(Error::NoDevice)?;
        let account_number = old_config.account_number.clone();
        let account_service = self.account_service.clone();
        Ok(async move {
            account_service
                .get_data_2(account_number)
                .await
                .map(|data| data.expiry)
        })
    }

    fn needs_validation(&mut self) -> bool {
        if !self.data.logged_in() {
            return true;
        }

        let now = SystemTime::now();

        let elapsed = self
            .last_validation
            .and_then(|last_check| now.duration_since(last_check).ok())
            .unwrap_or(VALIDITY_CACHE_TIMEOUT);

        if elapsed >= VALIDITY_CACHE_TIMEOUT {
            self.last_validation = Some(now);
            return true;
        }

        false
    }

    async fn shutdown(self) {
        self.cacher.finalize().await;
    }
}
pub struct DeviceCacher {
    file: io::BufWriter<fs::File>,
    path: std::path::PathBuf,
}

impl DeviceCacher {
    pub async fn new(settings_dir: &Path) -> Result<(DeviceCacher, PrivateDeviceState), Error> {
        let path = settings_dir.join(DEVICE_CACHE_FILENAME);
        let cache_exists = path.is_file();
        let mut should_save = false;

        let mut file = fs::OpenOptions::from(Self::file_options())
            .write(true)
            .read(true)
            .create(true)
            .open(&path)
            .await?;

        let device: PrivateDeviceState = if cache_exists {
            let mut reader = io::BufReader::new(&mut file);
            let mut buffer = String::new();
            reader.read_to_string(&mut buffer).await?;
            if !buffer.is_empty() {
                serde_json::from_str(&buffer).unwrap_or_else(|error| {
                    should_save = true;
                    log::error!(
                        "{}",
                        error.display_chain_with_msg("Wiping device config due to an error")
                    );
                    PrivateDeviceState::LoggedOut
                })
            } else {
                should_save = true;
                PrivateDeviceState::LoggedOut
            }
        } else {
            should_save = true;
            PrivateDeviceState::LoggedOut
        };

        let mut store = DeviceCacher {
            file: io::BufWriter::new(file),
            path,
        };

        if should_save {
            store.write(&device).await?;
        }

        Ok((store, device))
    }

    fn file_options() -> std::fs::OpenOptions {
        let mut options = std::fs::OpenOptions::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // exclusive access
            options.share_mode(0);
        }
        options
    }

    pub async fn write(&mut self, device: &PrivateDeviceState) -> Result<(), Error> {
        let data = serde_json::to_vec_pretty(&device).unwrap();

        self.file.get_mut().set_len(0).await?;
        self.file.seek(io::SeekFrom::Start(0)).await?;
        self.file.write_all(&data).await?;
        self.file.flush().await?;
        self.file.get_mut().sync_data().await?;

        Ok(())
    }

    pub async fn remove(self) -> Result<(), Error> {
        let path = {
            let DeviceCacher { path, file } = self;
            let std_file = file.into_inner().into_std().await;
            let _ = tokio::task::spawn_blocking(move || drop(std_file)).await;
            path
        };
        tokio::fs::remove_file(path).await?;
        Ok(())
    }

    async fn finalize(self) {
        let std_file = self.file.into_inner().into_std().await;
        let _ = tokio::task::spawn_blocking(move || drop(std_file)).await;
    }
}

/// Checks if the current device is valid if a WireGuard tunnel cannot be set up
/// after multiple attempts.
pub(crate) struct TunnelStateChangeHandler {
    manager: AccountManagerHandle,
    can_retry: Arc<AtomicBool>,
    wg_retry_attempt: usize,
}

impl TunnelStateChangeHandler {
    pub fn new(manager: AccountManagerHandle) -> Self {
        Self {
            manager,
            can_retry: Arc::new(AtomicBool::new(true)),
            wg_retry_attempt: 0,
        }
    }

    /// Handle state transitions and optionally check the device/account validity. This should be
    /// called during every tunnel state transition.
    pub fn handle_state_transition(&mut self, new_state: &TunnelStateTransition) {
        self.wg_retry_attempt = Self::update_retry_counter(new_state, self.wg_retry_attempt);
        Self::update_retry_bool(new_state, self.can_retry.clone());
        // Check if a device-check should be triggered
        if Self::should_check_device_validity(self.wg_retry_attempt, self.can_retry.clone()) {
            let handle = self.manager.clone();
            tokio::spawn(Self::check_device_validity(
                self.can_retry.clone(),
                move || Self::check_device_validity_inner(handle),
            ));
        }
    }

    /// Run `validate` when connecting to a WireGuard server.
    ///
    /// # Note
    /// `can_retry` is reset on network errors. Otherwise, it is set to `true` as to not
    /// immediately trigger new device checks.
    async fn check_device_validity<Validate, ValidateResult>(
        can_retry: Arc<AtomicBool>,
        validate: Validate,
    ) where
        Validate: FnOnce() -> ValidateResult + Send,
        ValidateResult: Future<Output = Result<(), Error>> + Send,
    {
        // Log any error
        let result = validate().await.inspect_err(|error| {
            log::error!(
                "{}",
                error.display_chain_with_msg("Failed to check device or account validity")
            )
        });
        // Update `can_retry` based on the result of `validate`
        match result {
            // If the request failed due to a network error, we should continue
            // retrying.
            Err(ref error) if Self::should_continue_retries(error) => {
                can_retry.store(true, Ordering::SeqCst);
            }
            // Otherwise we give up, because it means we have a known result or
            // the API returned some error.
            _ => (),
        }
    }

    /// Return an incremented count for `retry_attempt` if this is another WireGuard connection
    /// attempt, otherwise `retry_attempt` is returned.
    ///
    /// Reset to the counter to `0` when we manage to successfully connect to a Wireguard relay.
    fn update_retry_counter(new_state: &TunnelStateTransition, retry_attempt: usize) -> usize {
        let wireguard =
            |endpoint: &TunnelEndpoint| matches!(endpoint.tunnel_type, TunnelType::Wireguard);

        match new_state {
            // Increment the counter if this is another Wireguard attempt
            TunnelStateTransition::Connecting(endpoint) if wireguard(endpoint) => {
                retry_attempt.wrapping_add(1)
            }
            // Only reset the counter if we managed to connect to a Wireguard relay
            TunnelStateTransition::Connected(endpoint) if wireguard(endpoint) => 0,
            // Any other state transition doesn't affect the counter
            _ => retry_attempt,
        }
    }

    /// Check if `new_state` breaks a connecting-loop. If so, the retry state `can_retry` is reset
    /// (i.e. set to `true`).
    ///
    /// # Note
    /// The following state transition counts as breaking a connecting-loop: `Connected`,
    /// `Disconnected` and `Error`.
    fn update_retry_bool(new_state: &TunnelStateTransition, can_retry: Arc<AtomicBool>) {
        match new_state {
            TunnelStateTransition::Disconnected { .. }
            | TunnelStateTransition::Connected(_)
            | TunnelStateTransition::Error(_) => {
                can_retry.store(true, Ordering::SeqCst);
            }
            _ => {}
        };
    }

    async fn check_device_validity_inner(handle: AccountManagerHandle) -> Result<(), Error> {
        handle.validate_device().await?;
        handle.check_expiry().await.map(|_expiry| ())
    }

    /// Check if a device check is due
    fn should_check_device_validity(
        wireguard_retry_attempt: usize,
        can_retry: Arc<AtomicBool>,
    ) -> bool {
        Self::should_check_device_validity_on_attempt(wireguard_retry_attempt)
            && can_retry.swap(false, Ordering::SeqCst)
    }

    /// Check if a device check should be triggered based on the current `wireguard_retry_attempt`
    const fn should_check_device_validity_on_attempt(wireguard_retry_attempt: usize) -> bool {
        // Incorporate a debounce effect where every `WG_DEVICE_CHECK_THRESHOLD` attempt should be
        // able to trigger a device check.
        wireguard_retry_attempt > 0
            && wireguard_retry_attempt.is_multiple_of(WG_DEVICE_CHECK_THRESHOLD)
    }

    fn should_continue_retries(err: &Error) -> bool {
        err.is_network_error() || err.is_aborted()
    }
}

#[cfg(test)]
mod test {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use talpid_types::tunnel::TunnelStateTransition;

    use super::{Error, TunnelStateChangeHandler, WG_DEVICE_CHECK_THRESHOLD};

    const TIMEOUT_ERROR: Error = Error::OtherRestError(mullvad_api::rest::Error::TimeoutError);

    /// Verify that a device check is triggered 'when expected', i.e. when the current attempt
    /// has reached the threshold as specified by [`WG_DEVICE_CHECK_THRESHOLD`]
    #[test]
    fn test_device_check_by_retry_attempt() {
        assert!(
            TunnelStateChangeHandler::should_check_device_validity_on_attempt(
                WG_DEVICE_CHECK_THRESHOLD
            )
        );
    }

    /// Starting a new connection loop should resume device validity checks
    #[test]
    fn test_device_check_reset() {
        let can_retry = Arc::new(AtomicBool::new(false));
        // Transitioning to the 'Disconnected' state counts as breaking the 'connection loop'
        let new_tunnel_state = TunnelStateTransition::Disconnected { locked_down: false };
        TunnelStateChangeHandler::update_retry_bool(&new_tunnel_state, can_retry.clone());

        assert!(
            can_retry.load(Ordering::SeqCst),
            "expected retry state to be reset on first connection attempt"
        );
    }

    /// Retries should stop when a device check succeeds
    #[tokio::test]
    async fn test_device_check_on_success() {
        let can_retry = Arc::new(AtomicBool::new(true));

        let did_run = TunnelStateChangeHandler::should_check_device_validity(
            WG_DEVICE_CHECK_THRESHOLD,
            can_retry.clone(),
        );
        assert!(did_run, "expected device check to run");
        // Manually trigger the device check and verify that we still can try to perform a device
        // check
        TunnelStateChangeHandler::check_device_validity(can_retry.clone(), || async { Ok(()) })
            .await;

        let did_run = TunnelStateChangeHandler::should_check_device_validity(
            WG_DEVICE_CHECK_THRESHOLD,
            can_retry.clone(),
        );
        assert!(
            !did_run,
            "expected device check to give up after successful check"
        );
    }

    /// Retries should continue when a network error occurs
    #[tokio::test]
    async fn test_device_check_on_network_error() {
        let can_retry = Arc::new(AtomicBool::new(true));

        // Run the check with a (simulated) network error - verify that `can_retry` is still true
        // afterwards, indicating that a device check may still be performed
        TunnelStateChangeHandler::check_device_validity(can_retry.clone(), || async {
            Err(TIMEOUT_ERROR)
        })
        .await;

        assert!(
            can_retry.load(Ordering::SeqCst),
            "expected device check to continue after a network error"
        );

        // Re-run the check without a network error - verify that `can_retry` is no longer true
        TunnelStateChangeHandler::should_check_device_validity(
            WG_DEVICE_CHECK_THRESHOLD,
            can_retry.clone(),
        );

        TunnelStateChangeHandler::check_device_validity(can_retry.clone(), || async { Ok(()) })
            .await;

        assert!(
            !can_retry.load(Ordering::SeqCst),
            "device check should no longer happen after successful check"
        );
    }
}
