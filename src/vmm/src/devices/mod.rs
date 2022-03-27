// SPDX-License-Identifier: Apache-2.0

use std::convert::TryFrom;
use std::io;
use std::io::Error as IoError;
use std::ops::DerefMut;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use event_manager::{
    Error as EvmgrError, EventManager, MutEventSubscriber, RemoteEndpoint, Result as EvmgrResult,
    SubscriberId,
};
use kvm_ioctls::{IoEventAddress, VmFd};
use libc::EFD_NONBLOCK;
use linux_loader::cmdline::Cmdline;
use virtio_device::VirtioConfig;
use vm_device::bus::{MmioAddress, MmioRange};
use vm_device::device_manager::MmioManager;
use vm_device::DeviceMmio;
use vm_memory::{GuestAddress, GuestAddressSpace};
use vmm_sys_util::errno;
use vmm_sys_util::eventfd::EventFd;

mod bindings;
pub(crate) mod serial;
pub mod tap;

// Device-independent virtio features.
mod features {
    pub const VIRTIO_F_RING_EVENT_IDX: u64 = 29;
    pub const VIRTIO_F_VERSION_1: u64 = 32;
    pub const VIRTIO_F_IN_ORDER: u64 = 35;
}

// The driver will write to the register at this offset in the MMIO region to notify the device
// about available queue events.
const VIRTIO_MMIO_QUEUE_NOTIFY_OFFSET: u64 = 0x50;
const VIRTIO_QUEUE_MAX_SIZE: u16 = 256;

/// Custom defined [`std::result::Result`]
pub type Result<T> = std::result::Result<T, Error>;
/// Variable used to sub to EventManager updates
pub type Subscriber = Arc<Mutex<dyn MutEventSubscriber + Send>>;

/// Error related to MMIO / devices
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Cannot create a new Mmio Range")]
    Bus(vm_device::bus::Error),
    #[error("Cannot get next MMioConfig, memory overflow")]
    Overflow,

    #[error("Failed to open /dev/net/tun0")]
    OpenTun(IoError),

    #[error("Failed to communicate with device")]
    IoctlError(IoError),

    #[error("Could not configure kernel cmdline")]
    Cmdline(linux_loader::cmdline::Error),

    #[error("Failed to create event file descriptor")]
    EventFd(io::Error),

    #[error("Failed to register event to cmdline")]
    RegisterIoevent(errno::Error),

    #[error("Failed to register Interface Request FileDescriptor")]
    RegisterIrqfd(errno::Error),

    #[error("Could not validate virtio queues")]
    QueuesNotValid,

    #[error("Given device is already activated")]
    AlreadyActivated,

    #[error("Could not communicate with event manager remote endpoint")]
    Endpoint(EvmgrError),
}

#[derive(Copy, Clone)]
pub struct MmioConfig {
    pub range: MmioRange,
    // The interrupt assigned to the device.
    // Global System Interrupts can be thought of as ACPI Plug and Play IRQ numbers.
    // They are used to virtualize interrupts in tables and in ASL methods that perform resource allocation of interrupts.
    // https://stackoverflow.com/questions/45206171/how-sci-system-control-interrupt-vector-is-defined
    pub gsi: u32,
}

impl MmioConfig {
    pub fn new(base: u64, size: u64, gsi: u32) -> Result<Self> {
        MmioRange::new(MmioAddress(base), size)
            .map(|range| MmioConfig { range, gsi })
            .map_err(Error::Bus)
    }
}

// Represents the environment the devices in this crate current expect in order to be created
// and registered with the appropriate buses/handlers/etc. We're always passing a mmio_cfg object
// for now, and we'll re-evaluate the mechanism for exposing environment (i.e. maybe we'll do it
// through an object that implements a number of traits the devices are aware of).
pub struct Env<'a, M, B> {
    // The objects used for guest memory accesses and other operations.
    pub mem: M,
    // Used by the devices to register ioevents and irqfds.
    pub vm_fd: Arc<VmFd>,
    // Mutable handle to the event manager the device is supposed to register with. There could be
    // more if we decide to use more than just one thread for device model emulation.
    pub event_mgr: &'a mut EventManager<Arc<Mutex<dyn MutEventSubscriber + Send>>>,
    // This stands for something that implements `MmioManager`, and can be passed as a reference
    // or smart pointer (such as a `Mutex` guard).
    pub mmio_mgr: B,
    // The virtio MMIO device parameters (MMIO range and interrupt to be used).
    pub mmio_cfg: MmioConfig,
    // We pass a mutable reference to the kernel cmdline `String` so the device can add any
    // required arguments (i.e. for virtio over MMIO discovery). This means we need to create
    // the devices before loading he kernel cmdline into memory, but that's not a significant
    // limitation.
    pub kernel_cmdline: &'a mut Cmdline,
}

impl<'a, M, B> Env<'a, M, B>
where
    // We're using this (more convoluted) bound so we can pass both references and smart
    // pointers such as mutex guards here.
    B: DerefMut,
    B::Target: MmioManager<D = Arc<dyn DeviceMmio + Send + Sync>>,
{
    // Registers an MMIO device with the inner bus and kernel cmdline.
    pub fn register_mmio_device(
        &mut self,
        device: Arc<dyn DeviceMmio + Send + Sync>,
    ) -> Result<()> {
        self.mmio_mgr
            .register_mmio(self.mmio_cfg.range, device)
            .map_err(Error::Bus)?;

        self.kernel_cmdline
            .add_virtio_mmio_device(
                self.mmio_cfg.range.size(),
                GuestAddress(self.mmio_cfg.range.base().0),
                self.mmio_cfg.gsi,
                None,
            )
            .map_err(Error::Cmdline)?;

        Ok(())
    }

    // Appends a string to the inner kernel cmdline.
    pub fn insert_cmdline_str<T: AsRef<str>>(&mut self, t: T) -> Result<()> {
        self.kernel_cmdline
            .insert_str(t.as_ref())
            .map_err(Error::Cmdline)
    }
}

// Holds configuration objects which are common to all current devices.
pub struct CommonConfig<M: GuestAddressSpace> {
    pub virtio: VirtioConfig<M>,
    pub mmio: MmioConfig,
    pub endpoint: RemoteEndpoint<Subscriber>,
    pub vm_fd: Arc<VmFd>,
    pub irqfd: Arc<EventFd>,
}

impl<M: GuestAddressSpace> CommonConfig<M> {
    pub fn new<B>(virtio_cfg: VirtioConfig<M>, env: &Env<M, B>) -> Result<Self> {
        let irqfd = Arc::new(EventFd::new(EFD_NONBLOCK).map_err(Error::EventFd)?);

        env.vm_fd
            .register_irqfd(&irqfd, env.mmio_cfg.gsi)
            .map_err(Error::RegisterIrqfd)?;

        Ok(CommonConfig {
            virtio: virtio_cfg,
            mmio: env.mmio_cfg,
            endpoint: env.event_mgr.remote_endpoint(),
            vm_fd: env.vm_fd.clone(),
            irqfd,
        })
    }

    // Perform common initial steps for device activation based on the configuration, and return
    // a `Vec` that contains `EventFd`s registered as ioeventfds, which are used to convey queue
    // notifications coming from the driver.
    pub fn prepare_activate(&self) -> Result<Vec<EventFd>> {
        if !self.virtio.queues_valid() {
            return Err(Error::QueuesNotValid);
        }

        if self.virtio.device_activated {
            return Err(Error::AlreadyActivated);
        }

        let mut ioevents = Vec::new();

        // Right now, we operate under the assumption all queues are marked ready by the device
        // (which is true until we start supporting devices that can optionally make use of
        // additional queues on top of the defaults).
        for i in 0..self.virtio.queues.len() {
            // EventFd are file descriptor scoped to notify events
            let fd = EventFd::new(EFD_NONBLOCK).map_err(Error::EventFd)?;

            // Register the queue event fd, it means whenever something is written
            // in the mmio range, we get a notification through eventfd
            self.vm_fd
                .register_ioevent(
                    &fd,
                    &IoEventAddress::Mmio(
                        self.mmio.range.base().0 + VIRTIO_MMIO_QUEUE_NOTIFY_OFFSET,
                    ),
                    // The maximum number of queues should fit within an `u16` according to the
                    // standard, so the conversion below is always expected to succeed.
                    u32::try_from(i).unwrap(),
                )
                .map_err(Error::RegisterIoevent)?;

            ioevents.push(fd);
        }

        Ok(ioevents)
    }

    // Perform the final steps of device activation based on the inner configuration and the
    // provided subscriber that's going to handle the device queues. We'll extend this when
    // we start support devices that make use of multiple handlers (i.e. for multiple queues).
    pub fn finalize_activate(&mut self, handler: Subscriber) -> Result<()> {
        // Register the queue handler with the `EventManager`. We could record the `sub_id`
        // (and/or keep a handler clone) for further interaction (i.e. to remove the subscriber at
        // a later time, retrieve state, etc).
        let _sub_id = self
            .endpoint
            .call_blocking(move |mgr| -> EvmgrResult<SubscriberId> {
                Ok(mgr.add_subscriber(handler))
            })
            .map_err(Error::Endpoint)?;

        self.virtio.device_activated = true;

        Ok(())
    }
}

/// Simple trait to model the operation of signalling the driver about used events
/// for the specified queue.
pub trait SignalUsedQueue {
    // level so the expectation is the implementation handles that transparently somehow.
    fn signal_used_queue(&self, index: u16);
}

/// Uses a single irqfd as the basis of signalling any queue (useful for the MMIO transport,
/// where a single interrupt is shared for everything).
pub struct SingleFdSignalQueue {
    pub irqfd: Arc<EventFd>,
    pub interrupt_status: Arc<AtomicU8>,
}

impl SignalUsedQueue for SingleFdSignalQueue {
    fn signal_used_queue(&self, _index: u16) {
        // This bit is set on the device interrupt status when notifying the driver about used
        // queue events.
        self.interrupt_status.fetch_or(0x01, Ordering::SeqCst);
        self.irqfd
            .write(1)
            .expect("Failed write to eventfd when signalling queue");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use event_manager::{EventOps, Events};
    use virtio_queue::Queue;
    use vm_device::bus::MmioAddress;
    use vm_device::device_manager::IoManager;
    use vm_device::MutDeviceMmio;
    use vm_memory::{GuestAddress, GuestMemoryMmap};

    use super::features::VIRTIO_F_VERSION_1;
    use super::*;

    pub type MockMem = Arc<GuestMemoryMmap>;

    // Can be used in other modules to test functionality that requires a `CommonArgs` struct as
    // input. The `args` method below generates an instance of `CommonArgs` based on the members
    // below.
    pub struct EnvMock {
        pub mem: MockMem,
        pub vm_fd: Arc<VmFd>,
        pub event_mgr: EventManager<Arc<Mutex<dyn MutEventSubscriber + Send>>>,
        pub mmio_mgr: IoManager,
        pub mmio_cfg: MmioConfig,
        pub kernel_cmdline: Cmdline,
    }

    impl EnvMock {
        pub fn new() -> Self {
            let mem =
                Arc::new(GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000_0000)]).unwrap());

            let kvm = kvm_ioctls::Kvm::new().unwrap();
            let vm_fd = Arc::new(kvm.create_vm().unwrap());

            let range = MmioRange::new(MmioAddress(0x1_0000_0000), 0x1000).unwrap();
            let mmio_cfg = MmioConfig { range, gsi: 5 };

            // Required so the vm_fd can be used to register irqfds.
            vm_fd.create_irq_chip().unwrap();

            EnvMock {
                mem,
                vm_fd,
                event_mgr: EventManager::new().unwrap(),
                mmio_mgr: IoManager::new(),
                mmio_cfg,
                // `4096` seems large enough for testing.
                kernel_cmdline: Cmdline::new(4096),
            }
        }

        pub fn env(&mut self) -> Env<MockMem, &mut IoManager> {
            Env {
                mem: self.mem.clone(),
                vm_fd: self.vm_fd.clone(),
                event_mgr: &mut self.event_mgr,
                mmio_mgr: &mut self.mmio_mgr,
                mmio_cfg: self.mmio_cfg,
                kernel_cmdline: &mut self.kernel_cmdline,
            }
        }
    }

    // Skipping until adding Arm support and figuring out how make the irqchip creation in the
    // `Mock` object work there as well.
    #[cfg_attr(target_arch = "aarch64", ignore)]
    #[test]
    fn test_env() {
        // Just a dummy device we're going to register on the bus.
        struct Dummy;

        impl MutDeviceMmio for Dummy {
            fn mmio_read(&mut self, _base: MmioAddress, _offset: u64, _data: &mut [u8]) {}

            fn mmio_write(&mut self, _base: MmioAddress, _offset: u64, _data: &[u8]) {}
        }

        let mut mock = EnvMock::new();

        let dummy = Arc::new(Mutex::new(Dummy));

        mock.env().register_mmio_device(dummy).unwrap();

        let range = mock.mmio_cfg.range;
        let bus_range = mock.mmio_mgr.mmio_device(range.base()).unwrap().0;
        assert_eq!(bus_range.base(), range.base());
        assert_eq!(bus_range.size(), range.size());

        assert_eq!(
            mock.kernel_cmdline.as_str(),
            format!(
                "virtio_mmio.device=4K@0x{:x}:{}",
                range.base().0,
                mock.mmio_cfg.gsi
            )
        );

        mock.env().insert_cmdline_str("ending_string").unwrap();
        assert!(mock.kernel_cmdline.as_str().ends_with("ending_string"));
    }
}
