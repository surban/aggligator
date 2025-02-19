#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    html_favicon_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    issue_tracker_base_url = "https://github.com/surban/aggligator/issues/"
)]

//! [Aggligator](aggligator) transport: WebUSB for targeting WebAssembly
//!
//! This uses a [USB packet channel (UPC)](upc) to encapsulate data over a WebUSB connection.

use async_trait::async_trait;
use bimap::BiHashMap;
use futures::{StreamExt, TryStreamExt};
use std::{
    any::Any,
    cmp::Ordering,
    collections::HashSet,
    fmt,
    hash::{Hash, Hasher},
    io::{Error, ErrorKind, Result},
    rc::Rc,
    sync::Mutex,
    time::Duration,
};
use tokio::sync::watch;

use aggligator::{
    control::Direction,
    io::{StreamBox, TxRxBox},
    transport::{ConnectingTransport, LinkTag, LinkTagBox},
};

use webusb_web::{Usb, UsbDevice};

mod thread_bound;
use thread_bound::ThreadBound;

mod sleep;
use sleep::JsSleep;

static NAME: &str = "webusb";
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Link tag for outgoing WebUSB link.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutgoingWebUsbLinkTag {
    /// Id.
    id: u32,
    /// Interface number.
    interface: u8,
}

impl OutgoingWebUsbLinkTag {
    /// Internal USB device id.
    ///
    /// This is not persistent.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Interface number.
    pub fn interface(&self) -> u8 {
        self.interface
    }
}

impl fmt::Display for OutgoingWebUsbLinkTag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "USB -> {}:{}", self.id, self.interface)
    }
}

impl LinkTag for OutgoingWebUsbLinkTag {
    fn transport_name(&self) -> &str {
        NAME
    }

    fn direction(&self) -> Direction {
        Direction::Outgoing
    }

    fn user_data(&self) -> Vec<u8> {
        Vec::new()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn box_clone(&self) -> LinkTagBox {
        Box::new(self.clone())
    }

    fn dyn_cmp(&self, other: &dyn LinkTag) -> Ordering {
        let other = other.as_any().downcast_ref::<Self>().unwrap();
        Ord::cmp(self, other)
    }

    fn dyn_hash(&self, mut state: &mut dyn Hasher) {
        Hash::hash(self, &mut state)
    }
}

/// WebUSB device information.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeviceInfo {
    /// Vendor id.
    pub vendor_id: u16,
    /// Product id.
    pub product_id: u16,
    /// Class code.
    pub class_code: u8,
    /// Sub class code.
    pub sub_class_code: u8,
    /// Protocol code.
    pub protocol_code: u8,
    /// Manufacturer.
    pub manufacturer: Option<String>,
    /// Product.
    pub product: Option<String>,
    /// Serial number.
    pub serial_number: Option<String>,
}

/// WebUSB interface information.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InterfaceInfo {
    /// Interface number.
    pub number: u8,
    /// Class code.
    pub class_code: u8,
    /// Sub class code.
    pub sub_class_code: u8,
    /// Protocol code.
    pub protocol_code: u8,
    /// Description.
    pub description: Option<String>,
}

type FilterFn = Box<dyn Fn(&DeviceInfo, &InterfaceInfo) -> bool + Send + Sync>;

/// WebUSB transport for outgoing connections.
///
/// This transport is packet-based.
///
/// ### USB device access permission
/// The user needs to grant access to a USB device before it will be detected by this transport.
/// You can use [`webusb_web::Usb::request_device`] or any other method to call
/// [`USB.requestDevice()`](https://developer.mozilla.org/en-US/docs/Web/API/USB/requestDevice)
/// in order to prompt the user for consent.
pub struct WebUsbConnector {
    usb: ThreadBound<Usb>,
    known_devices: Mutex<KnownDevices>,
    filter: FilterFn,
}

#[derive(Default)]
struct KnownDevices {
    devices: BiHashMap<u32, ThreadBound<UsbDevice>>,
    next_id: u32,
}

impl fmt::Debug for WebUsbConnector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("WebUsbConnector").finish_non_exhaustive()
    }
}

impl WebUsbConnector {
    /// Creates a new WebUSB transport for outgoing connections.
    ///
    /// The `filter` function is called for each discovered USB device and should return `true` if the target
    /// USB device and interface is matched.
    ///
    /// USB devices are re-enumerated when a hotplug event occurs.
    pub fn new(filter: impl Fn(&DeviceInfo, &InterfaceInfo) -> bool + Send + Sync + 'static) -> Result<Self> {
        Ok(Self {
            usb: ThreadBound::new(Usb::new()?),
            known_devices: Mutex::new(KnownDevices::default()),
            filter: Box::new(filter),
        })
    }

    /// Local id for specified device.
    fn device_id(&self, device: &UsbDevice) -> u32 {
        let mut known_devices = self.known_devices.lock().unwrap();
        match known_devices.devices.get_by_right(device) {
            Some(id) => *id,
            None => {
                let id = known_devices.next_id;
                known_devices.next_id += 1;
                known_devices.devices.insert(id, ThreadBound::new(device.clone()));
                id
            }
        }
    }

    /// Get device by local id.
    fn device_by_id(&self, id: u32) -> Option<UsbDevice> {
        let known_devices = self.known_devices.lock().unwrap();
        known_devices.devices.get_by_left(&id).map(|d| ThreadBound::into_inner(d.clone()))
    }
}

#[async_trait]
impl ConnectingTransport for WebUsbConnector {
    fn name(&self) -> &str {
        NAME
    }

    async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()> {
        ThreadBound::new(async move {
            let mut events = self.usb.events();

            loop {
                let mut tags: HashSet<LinkTagBox> = HashSet::new();

                for dev in self.usb.devices().await {
                    let id = self.device_id(&dev);
                    let Some(cfg) = dev.configuration() else { continue };

                    let device_info = DeviceInfo {
                        vendor_id: dev.vendor_id(),
                        product_id: dev.product_id(),
                        class_code: dev.device_class(),
                        sub_class_code: dev.device_subclass(),
                        protocol_code: dev.device_protocol(),
                        manufacturer: dev.manufacturer_name(),
                        product: dev.product_name(),
                        serial_number: dev.serial_number(),
                    };

                    for iface in cfg.interfaces {
                        let alt = iface.alternate;
                        let interface_info = InterfaceInfo {
                            number: alt.alternate_setting,
                            class_code: alt.interface_class,
                            sub_class_code: alt.interface_subclass,
                            protocol_code: alt.interface_protocol,
                            description: alt.interface_name,
                        };

                        if (self.filter)(&device_info, &interface_info) {
                            tags.insert(Box::new(OutgoingWebUsbLinkTag {
                                id,
                                interface: iface.interface_number,
                            }));
                        }
                    }
                }

                tx.send_if_modified(|v| {
                    if *v != tags {
                        *v = tags;
                        true
                    } else {
                        false
                    }
                });

                tokio::select! {
                    res = events.next() => {
                        if res.is_none() {
                            break;
                        }
                    }
                    () = JsSleep::new(POLL_INTERVAL) => (),
                }
            }

            Ok(())
        })
        .await
    }

    async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox> {
        ThreadBound::new(async move {
            let tag: &OutgoingWebUsbLinkTag = tag.as_any().downcast_ref().unwrap();

            let Some(dev) = self.device_by_id(tag.id) else {
                return Err(Error::new(ErrorKind::NotFound, "USB device gone"));
            };

            let hnd = Rc::new(dev.open().await?);
            let (tx, rx) = upc::host::connect(hnd, tag.interface, &[]).await?;

            Ok(TxRxBox::new(tx.into_sink(), rx.into_stream().map_ok(|p| p.freeze())).into())
        })
        .await
    }
}
