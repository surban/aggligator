#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    html_favicon_url = "https://raw.githubusercontent.com/surban/aggligator/master/.misc/aggligator.png",
    issue_tracker_base_url = "https://github.com/surban/aggligator/issues/"
)]

//! [Aggligator](aggligator) transport: USB
//!
//! This uses a [USB packet channel (UPC)](upc) to encapsulate data over a USB connection.

use std::time::Duration;

static NAME: &str = "usb";
const TIMEOUT: Duration = Duration::from_secs(1);

#[cfg(feature = "host")]
#[cfg_attr(docsrs, doc(cfg(feature = "host")))]
mod host {
    use aggligator::io::{StreamBox, TxRxBox};
    use async_trait::async_trait;
    use futures::{FutureExt, StreamExt};
    use std::{
        any::Any,
        cmp::Ordering,
        collections::HashSet,
        fmt,
        hash::{Hash, Hasher},
        io::{Error, ErrorKind, Result},
        time::Duration,
    };
    use tokio::{
        sync::{watch, Mutex},
        time::sleep,
    };

    use aggligator::{
        control::Direction,
        transport::{ConnectingTransport, LinkTag, LinkTagBox},
    };

    use super::{NAME, TIMEOUT};

    const PROBE_INTERVAL: Duration = Duration::from_secs(3);

    /// Link tag for outgoing USB link.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct OutgoingUsbLinkTag {
        /// Bus id.
        pub bus_id: String,
        /// Device address.
        pub address: u8,
        /// Interface number.
        pub interface: u8,
    }

    impl fmt::Display for OutgoingUsbLinkTag {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "USB {} -> {}:{}", self.bus_id, self.address, self.interface)
        }
    }

    impl LinkTag for OutgoingUsbLinkTag {
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

    /// USB device information.
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct DeviceInfo {
        /// Bus id.
        pub bus_id: String,
        /// Bus number. (Linux only)
        #[cfg(any(target_os = "linux", target_os = "android"))]
        pub busnum: u8,
        /// Address.
        pub address: u8,
        /// USB port number chain.
        pub port_numbers: Vec<u8>,
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
        /// Device version.
        pub version: u16,
        /// USB version.
        pub usb_version: u16,
        /// Manufacturer.
        pub manufacturer: Option<String>,
        /// Product.
        pub product: Option<String>,
        /// Serial number.
        pub serial_number: Option<String>,
    }

    /// USB interface information.
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

    /// USB transport for outgoing connections.
    ///
    /// This transport is packet-based.
    pub struct UsbConnector {
        filter: FilterFn,
        hotplug: Option<Mutex<nusb::hotplug::HotplugWatch>>,
    }

    impl fmt::Debug for UsbConnector {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.debug_struct("UsbConnector").finish_non_exhaustive()
        }
    }

    impl UsbConnector {
        /// Creates a new USB transport for outgoing connections.
        ///
        /// The `filter` function is called for each discovered USB device and should return `true` if the target
        /// USB device and interface is matched.
        ///
        /// USB devices are re-enumerated when a hotplug event occurs, or, if hotplug events are unsupported
        /// by the operating system, periodically.
        pub fn new(filter: impl Fn(&DeviceInfo, &InterfaceInfo) -> bool + Send + Sync + 'static) -> Result<Self> {
            let hotplug = match nusb::watch_devices() {
                Ok(hotplug) => Some(Mutex::new(hotplug)),
                Err(err) => {
                    tracing::warn!("USB hotplug detection not available: {err}");
                    None
                }
            };

            Ok(Self { filter: Box::new(filter), hotplug })
        }

        async fn probe_device(&self, dev_info: &nusb::DeviceInfo) -> Result<Vec<OutgoingUsbLinkTag>> {
            let dev = dev_info.open().await?;
            let cfg = dev.active_configuration()?;
            let desc = dev.device_descriptor();

            let lang = match dev.get_string_descriptor_supported_languages(TIMEOUT).await {
                Ok(mut langs) => langs.next(),
                Err(err) => {
                    tracing::warn!("cannot get string descriptor languages: {err}");
                    None
                }
            };

            let read_desc = async |desc_index| {
                let desc_index = desc_index?;
                match lang {
                    Some(lang) => match dev.get_string_descriptor(desc_index, lang, TIMEOUT).await {
                        Ok(s) => Some(s),
                        Err(err) => {
                            tracing::warn!("cannot read string descriptor {desc_index}: {err}");
                            None
                        }
                    },
                    None => None,
                }
            };

            let device_info = DeviceInfo {
                bus_id: dev_info.bus_id().to_string(),
                #[cfg(any(target_os = "linux", target_os = "android"))]
                busnum: dev_info.busnum(),
                address: dev_info.device_address(),
                port_numbers: dev_info.port_chain().to_vec(),
                vendor_id: dev_info.vendor_id(),
                product_id: dev_info.product_id(),
                class_code: dev_info.class(),
                sub_class_code: dev_info.subclass(),
                protocol_code: dev_info.protocol(),
                version: dev_info.device_version(),
                usb_version: dev_info.usb_version(),
                manufacturer: read_desc(desc.manufacturer_string_index()).await,
                product: read_desc(desc.product_string_index()).await,
                serial_number: read_desc(desc.serial_number_string_index()).await,
            };

            let mut tags = Vec::new();

            for iface in cfg.interfaces() {
                let desc = iface.first_alt_setting();

                let interface_info = InterfaceInfo {
                    number: desc.interface_number(),
                    class_code: desc.class(),
                    sub_class_code: desc.subclass(),
                    protocol_code: desc.protocol(),
                    description: read_desc(desc.string_index()).await,
                };

                if (self.filter)(&device_info, &interface_info) {
                    tags.push(OutgoingUsbLinkTag {
                        bus_id: dev_info.bus_id().to_string(),
                        address: dev_info.device_address(),
                        interface: desc.interface_number(),
                    });
                }
            }

            Ok(tags)
        }
    }

    #[async_trait]
    impl ConnectingTransport for UsbConnector {
        fn name(&self) -> &str {
            NAME
        }

        async fn link_tags(&self, tx: watch::Sender<HashSet<LinkTagBox>>) -> Result<()> {
            loop {
                let mut tags = HashSet::new();
                for dev_info in nusb::list_devices().await? {
                    match self.probe_device(&dev_info).await {
                        Ok(dev_tags) => {
                            tags.extend(dev_tags.into_iter().map(|tag| Box::new(tag) as Box<dyn LinkTag>))
                        }
                        Err(err) => {
                            tracing::trace!(
                                "cannot probe device {}-{}: {err}",
                                dev_info.bus_id(),
                                dev_info.device_address()
                            )
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

                match &self.hotplug {
                    Some(hotplug) => {
                        let mut hotplug = hotplug.lock().await;
                        tokio::select! {
                            Some(_) = hotplug.next() => {
                                tracing::debug!("USB devices changed");
                                sleep(Duration::from_millis(100)).await;
                                while let Some(Some(_)) = hotplug.next().now_or_never() {}

                            }
                            () = sleep(PROBE_INTERVAL) => (),
                        }
                    }
                    None => sleep(PROBE_INTERVAL).await,
                }
            }
        }

        async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox> {
            let tag: &OutgoingUsbLinkTag = tag.as_any().downcast_ref().unwrap();

            let Some(dev) = nusb::list_devices()
                .await?
                .find(|cand| cand.bus_id() == tag.bus_id && cand.device_address() == tag.address)
            else {
                return Err(Error::new(ErrorKind::NotFound, "USB device gone"));
            };

            let dev = dev.open().await?;
            let (tx, rx) = upc::host::connect(dev, tag.interface, &[]).await?;

            Ok(TxRxBox::new(tx.into_sink(), rx.into_stream()).into())
        }
    }
}

#[cfg(feature = "host")]
#[cfg_attr(docsrs, doc(cfg(feature = "host")))]
pub use host::*;

#[cfg(feature = "device")]
#[cfg_attr(docsrs, doc(cfg(feature = "device")))]
mod device {
    use aggligator::{control::Direction, io::TxRxBox};
    use async_trait::async_trait;
    use core::fmt;
    use futures::TryStreamExt;
    use std::{
        any::Any,
        cmp::Ordering,
        ffi::{OsStr, OsString},
        hash::{Hash, Hasher},
        io::Result,
    };
    use tokio::sync::{mpsc, Mutex};
    use upc::device::UpcFunction;

    use aggligator::transport::{AcceptedStreamBox, AcceptingTransport, LinkTag, LinkTagBox};

    use super::NAME;

    pub use upc;
    pub use usb_gadget;

    /// Link tag for incoming USB link.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct IncomingUsbLinkTag {
        /// USB device controller name.
        pub udc: OsString,
    }

    impl fmt::Display for IncomingUsbLinkTag {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "UDC <- {}", self.udc.to_string_lossy())
        }
    }

    impl LinkTag for IncomingUsbLinkTag {
        fn transport_name(&self) -> &str {
            NAME
        }

        fn direction(&self) -> Direction {
            Direction::Incoming
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

    /// USB transport for incoming connections.
    ///
    /// This transport is packet-based.
    #[derive(Debug)]
    pub struct UsbAcceptor {
        upc_function: Mutex<UpcFunction>,
        udc_name: OsString,
    }

    impl UsbAcceptor {
        /// Creates a new USB transport accepting incoming connections from `upc_function`.
        ///
        /// `udc_name` specifies the name of the USB device controller (UDC).
        pub fn new(upc_function: UpcFunction, udc_name: impl AsRef<OsStr>) -> Self {
            Self { upc_function: Mutex::new(upc_function), udc_name: udc_name.as_ref().to_os_string() }
        }
    }

    #[async_trait]
    impl AcceptingTransport for UsbAcceptor {
        fn name(&self) -> &str {
            NAME
        }

        async fn listen(&self, conn_tx: mpsc::Sender<AcceptedStreamBox>) -> Result<()> {
            let mut upc_function = self.upc_function.lock().await;

            loop {
                let (tx, rx) = upc_function.accept().await?;
                let tx_rx = TxRxBox::new(tx.into_sink(), rx.into_stream().map_ok(|p| p.freeze()));

                let tag = IncomingUsbLinkTag { udc: self.udc_name.clone() };

                if conn_tx.send(AcceptedStreamBox::new(tx_rx.into(), tag)).await.is_err() {
                    break;
                }
            }

            Ok(())
        }
    }
}

#[cfg(feature = "device")]
#[cfg_attr(docsrs, doc(cfg(feature = "device")))]
pub use device::*;
