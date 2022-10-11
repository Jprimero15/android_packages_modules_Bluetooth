//! Suspend/Resume API.

use crate::bluetooth::{Bluetooth, BtifBluetoothCallbacks};
use crate::callbacks::Callbacks;
use crate::{bluetooth_gatt::IBluetoothGatt, BluetoothGatt, Message, RPCProxy};
use bt_topshim::btif::BluetoothInterface;
use log::warn;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::Sender;

/// Defines the Suspend/Resume API.
///
/// This API is exposed by `btadapterd` and independent of the suspend/resume detection mechanism
/// which depends on the actual operating system the daemon runs on. Possible clients of this API
/// include `btmanagerd` with Chrome OS `powerd` integration, `btmanagerd` with systemd Inhibitor
/// interface, or any script hooked to suspend/resume events.
pub trait ISuspend {
    /// Adds an observer to suspend events.
    ///
    /// Returns true if the callback can be registered.
    fn register_callback(&mut self, callback: Box<dyn ISuspendCallback + Send>) -> bool;

    /// Removes an observer to suspend events.
    ///
    /// Returns true if the callback can be removed, false if `callback_id` is not recognized.
    fn unregister_callback(&mut self, callback_id: u32) -> bool;

    /// Prepares the stack for suspend, identified by `suspend_id`.
    ///
    /// Returns a positive number identifying the suspend if it can be started. If there is already
    /// a suspend, that active suspend id is returned.
    fn suspend(&mut self, suspend_type: SuspendType);

    /// Undoes previous suspend preparation identified by `suspend_id`.
    ///
    /// Returns true if suspend can be resumed, and false if there is no suspend to resume.
    fn resume(&self) -> bool;
}

/// Suspend events.
pub trait ISuspendCallback: RPCProxy {
    /// Triggered when a callback is registered and given an identifier `callback_id`.
    fn on_callback_registered(&self, callback_id: u32);

    /// Triggered when the stack is ready for suspend and tell the observer the id of the suspend.
    fn on_suspend_ready(&self, suspend_id: u32);

    /// Triggered when the stack has resumed the previous suspend.
    fn on_resumed(&self, suspend_id: i32);
}

#[derive(FromPrimitive, ToPrimitive)]
#[repr(u32)]
pub enum SuspendType {
    NoWakesAllowed,
    AllowWakeFromHid,
    Other,
}

struct SuspendState {
    le_rand_expected: bool,
}

/// Implementation of the suspend API.
pub struct Suspend {
    bt: Arc<Mutex<Box<Bluetooth>>>,
    intf: Arc<Mutex<BluetoothInterface>>,
    gatt: Arc<Mutex<Box<BluetoothGatt>>>,
    tx: Sender<Message>,
    callbacks: Callbacks<dyn ISuspendCallback + Send>,
    is_connected_suspend: bool,
    was_a2dp_connected: bool,
    suspend_timeout_joinhandle: Option<tokio::task::JoinHandle<()>>,
    suspend_state: Arc<Mutex<SuspendState>>,
}

impl Suspend {
    pub fn new(
        bt: Arc<Mutex<Box<Bluetooth>>>,
        intf: Arc<Mutex<BluetoothInterface>>,
        gatt: Arc<Mutex<Box<BluetoothGatt>>>,
        tx: Sender<Message>,
    ) -> Suspend {
        Self {
            bt,
            intf,
            gatt,
            tx: tx.clone(),
            callbacks: Callbacks::new(tx.clone(), Message::SuspendCallbackDisconnected),
            is_connected_suspend: false,
            was_a2dp_connected: false,
            suspend_timeout_joinhandle: None,
            suspend_state: Arc::new(Mutex::new(SuspendState { le_rand_expected: false })),
        }
    }

    pub(crate) fn callback_registered(&mut self, id: u32) {
        match self.callbacks.get_by_id(id) {
            Some(callback) => callback.on_callback_registered(id),
            None => warn!("Suspend callback {} does not exist", id),
        }
    }

    pub(crate) fn remove_callback(&mut self, id: u32) -> bool {
        self.callbacks.remove_callback(id)
    }

    pub(crate) fn suspend_ready(&self, suspend_id: u32) {
        self.callbacks.for_all_callbacks(|callback| {
            callback.on_suspend_ready(suspend_id);
        });
    }
}

impl ISuspend for Suspend {
    fn register_callback(&mut self, callback: Box<dyn ISuspendCallback + Send>) -> bool {
        let id = self.callbacks.add_callback(callback);

        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _result = tx.send(Message::SuspendCallbackRegistered(id)).await;
        });

        true
    }

    fn unregister_callback(&mut self, callback_id: u32) -> bool {
        self.remove_callback(callback_id)
    }

    fn suspend(&mut self, suspend_type: SuspendType) {
        // self.was_a2dp_connected = TODO(230604670): check if A2DP is connected
        // self.current_advertiser_ids = TODO(224603198): save all advertiser ids
        self.intf.lock().unwrap().clear_event_mask();
        self.intf.lock().unwrap().clear_event_filter();
        self.intf.lock().unwrap().clear_filter_accept_list();
        // self.gatt.lock().unwrap().advertising_disable(); TODO(224602924): suspend all adv.
        self.gatt.lock().unwrap().stop_scan(0);
        self.intf.lock().unwrap().disconnect_all_acls();

        // Handle wakeful cases (Connected/Other)
        // Treat Other the same as Connected
        match suspend_type {
            SuspendType::AllowWakeFromHid => {
                // TODO(231345733): API For allowing classic HID only
                // TODO(230604670): check if A2DP is connected
                // TODO(224603198): save all advertiser information
            }
            SuspendType::NoWakesAllowed => {
                self.intf.lock().unwrap().clear_event_filter();
                self.intf.lock().unwrap().clear_event_mask();
            }
            _ => {
                self.intf.lock().unwrap().allow_wake_by_hid();
            }
        }
        self.intf.lock().unwrap().clear_filter_accept_list();
        self.intf.lock().unwrap().disconnect_all_acls();

        self.bt.lock().unwrap().le_rand();
        self.suspend_state.lock().unwrap().le_rand_expected = true;

        if let Some(join_handle) = &self.suspend_timeout_joinhandle {
            join_handle.abort();
            self.suspend_timeout_joinhandle = None;
        }

        let tx = self.tx.clone();
        let suspend_state = self.suspend_state.clone();
        self.suspend_timeout_joinhandle = Some(tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
            log::error!("Suspend did not complete in 2 seconds, continuing anyway.");

            suspend_state.lock().unwrap().le_rand_expected = false;
            tokio::spawn(async move {
                let _result = tx.send(Message::SuspendReady(1)).await;
            });
        }));
    }

    fn resume(&self) -> bool {
        self.intf.lock().unwrap().set_default_event_mask();
        self.intf.lock().unwrap().set_event_filter_inquiry_result_all_devices();
        self.intf.lock().unwrap().set_event_filter_connection_setup_all_devices();
        if self.is_connected_suspend {
            if self.was_a2dp_connected {
                // TODO(230604670): self.intf.lock().unwrap().restore_filter_accept_list();
                // TODO(230604670): reconnect to a2dp device
            }
            // TODO(224603198): start all advertising again
        }

        self.callbacks.for_all_callbacks(|callback| {
            callback.on_resumed(1);
        });

        true
    }
}

impl BtifBluetoothCallbacks for Suspend {
    fn le_rand_cb(&mut self, _random: u64) {
        // TODO(b/232547719): Suspend readiness may not depend only on LeRand, make a generic state
        // machine to support waiting for other conditions.
        if !self.suspend_state.lock().unwrap().le_rand_expected {
            log::warn!("Unexpected LE Rand callback, ignoring.");
            return;
        }

        if let Some(join_handle) = &self.suspend_timeout_joinhandle {
            join_handle.abort();
            self.suspend_timeout_joinhandle = None;
        }

        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _result = tx.send(Message::SuspendReady(1)).await;
        });
    }
}
