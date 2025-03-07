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
    use futures::TryStreamExt;
    use rusb::{Context, Device, Hotplug, HotplugBuilder, Registration, UsbContext};
    use std::{
        any::Any,
        cmp::Ordering,
        collections::HashSet,
        fmt,
        hash::{Hash, Hasher},
        io::{Error, ErrorKind, Result},
        sync::Arc,
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
        /// Bus number.
        pub bus: u8,
        /// Device address.
        pub address: u8,
        /// Interface number.
        pub interface: u8,
    }

    impl fmt::Display for OutgoingUsbLinkTag {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "USB {} -> {}:{}", self.bus, self.address, self.interface)
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
        /// Bus number.
        pub bus_number: u8,
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
        /// Manufacturer.
        pub manufacturer: String,
        /// Product.
        pub product: String,
        /// Serial number.
        pub serial_number: String,
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
        pub description: String,
    }

    struct HotplugCallback(watch::Sender<()>);

    impl Hotplug<Context> for HotplugCallback {
        fn device_arrived(&mut self, _device: Device<Context>) {
            self.0.send_replace(());
        }

        fn device_left(&mut self, _device: Device<Context>) {
            self.0.send_replace(());
        }
    }

    type FilterFn = Box<dyn Fn(&DeviceInfo, &InterfaceInfo) -> bool + Send + Sync>;

    /// USB transport for outgoing connections.
    ///
    /// This transport is packet-based.
    pub struct UsbConnector {
        context: Context,
        filter: FilterFn,
        _hotplug_reg: Option<Mutex<Registration<Context>>>,
        changed_rx: Option<watch::Receiver<()>>,
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
            let context = Context::new().map_err(Error::other)?;

            let (_hotplug_reg, changed_rx) = if rusb::has_hotplug() {
                let (changed_tx, changed_rx) = watch::channel(());
                let hotplug_reg = HotplugBuilder::new()
                    .register(&context, Box::new(HotplugCallback(changed_tx)))
                    .map_err(Error::other)?;
                (Some(Mutex::new(hotplug_reg)), Some(changed_rx))
            } else {
                (None, None)
            };

            Ok(Self { context, filter: Box::new(filter), _hotplug_reg, changed_rx })
        }

        fn probe_device(&self, dev: &Device<Context>) -> rusb::Result<Vec<OutgoingUsbLinkTag>> {
            let hnd = dev.open()?;
            let cfg = dev.active_config_descriptor()?;
            let desc = dev.device_descriptor()?;

            let langs = hnd.read_languages(TIMEOUT)?;
            let Some(&lang) = langs.first() else { return Err(rusb::Error::NotFound) };

            let device_info = DeviceInfo {
                bus_number: dev.bus_number(),
                address: dev.address(),
                port_numbers: dev.port_numbers()?,
                vendor_id: desc.vendor_id(),
                product_id: desc.product_id(),
                class_code: desc.class_code(),
                sub_class_code: desc.sub_class_code(),
                protocol_code: desc.protocol_code(),
                manufacturer: hnd.read_manufacturer_string(lang, &desc, TIMEOUT)?,
                product: hnd.read_product_string(lang, &desc, TIMEOUT)?,
                serial_number: hnd.read_serial_number_string(lang, &desc, TIMEOUT)?,
            };

            let mut tags = Vec::new();

            for iface in cfg.interfaces() {
                let Some(desc) = iface.descriptors().next() else { break };

                let interface_info = InterfaceInfo {
                    number: desc.interface_number(),
                    class_code: desc.class_code(),
                    sub_class_code: desc.sub_class_code(),
                    protocol_code: desc.protocol_code(),
                    description: hnd.read_interface_string(lang, &desc, TIMEOUT)?,
                };

                if (self.filter)(&device_info, &interface_info) {
                    tags.push(OutgoingUsbLinkTag {
                        bus: dev.bus_number(),
                        address: dev.address(),
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
            let mut changed_rx = self.changed_rx.clone();

            loop {
                if let Some(changed_rx) = &mut changed_rx {
                    changed_rx.borrow_and_update();
                }

                {
                    let devs = self.context.devices().map_err(Error::other)?;
                    let mut tags = HashSet::new();
                    for dev in devs.iter() {
                        match self.probe_device(&dev) {
                            Ok(dev_tags) => {
                                tags.extend(dev_tags.into_iter().map(|tag| Box::new(tag) as Box<dyn LinkTag>))
                            }
                            Err(err) => {
                                tracing::trace!(
                                    "cannot probe device {}-{}: {err}",
                                    dev.bus_number(),
                                    dev.address()
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
                }

                match &mut changed_rx {
                    Some(changed_rx) => {
                        tokio::select! {
                            Ok(()) = changed_rx.changed() => tracing::debug!("USB devices changed"),
                            () = sleep(PROBE_INTERVAL) => (),
                        }
                    }
                    None => sleep(PROBE_INTERVAL).await,
                }
            }
        }

        async fn connect(&self, tag: &dyn LinkTag) -> Result<StreamBox> {
            let tag: &OutgoingUsbLinkTag = tag.as_any().downcast_ref().unwrap();

            let mut dev = None;
            {
                let devs = self.context.devices().map_err(Error::other)?;
                for cand in devs.iter() {
                    if cand.bus_number() == tag.bus && cand.address() == tag.address {
                        dev = Some(cand);
                        break;
                    }
                }
            }
            let Some(dev) = dev else { return Err(Error::new(ErrorKind::NotFound, "USB device gone")) };

            let hnd = Arc::new(dev.open().map_err(Error::other)?);
            let (tx, rx) = upc::host::connect(hnd, tag.interface, &[]).await?;

            Ok(TxRxBox::new(tx.into_sink(), rx.into_stream().map_ok(|p| p.freeze())).into())
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
