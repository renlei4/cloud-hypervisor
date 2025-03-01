// Copyright 2019 Intel Corporation. All Rights Reserved.
//
// Portions Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
//
// SPDX-License-Identifier: (Apache-2.0 AND BSD-3-Clause)

use libc::{self, EFD_NONBLOCK};
use log::*;
use net_util::CtrlQueue;
use net_util::{
    open_tap, MacAddr, NetCounters, NetQueuePair, OpenTapError, RxVirtio, Tap, TxVirtio,
};
use option_parser::Toggle;
use option_parser::{OptionParser, OptionParserError};
use std::fmt;
use std::io::{self};
use std::net::Ipv4Addr;
use std::os::unix::io::AsRawFd;
use std::process;
use std::sync::{Arc, Mutex, RwLock};
use std::vec::Vec;
use vhost::vhost_user::message::*;
use vhost::vhost_user::Listener;
use vhost_user_backend::{VhostUserBackend, VhostUserDaemon, Vring, VringWorker};
use virtio_bindings::bindings::virtio_net::*;
use virtio_bindings::bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use vm_memory::{GuestAddressSpace, GuestMemoryAtomic, GuestMemoryMmap};
use vmm_sys_util::eventfd::EventFd;

pub type Result<T> = std::result::Result<T, Error>;
type VhostUserBackendResult<T> = std::result::Result<T, std::io::Error>;

#[derive(Debug)]
pub enum Error {
    /// Failed to create kill eventfd
    CreateKillEventFd(io::Error),
    /// Failed to parse configuration string
    FailedConfigParse(OptionParserError),
    /// Failed to signal used queue.
    FailedSignalingUsedQueue(io::Error),
    /// Failed to handle event other than input event.
    HandleEventNotEpollIn,
    /// Failed to handle unknown event.
    HandleEventUnknownEvent,
    /// Open tap device failed.
    OpenTap(OpenTapError),
    /// No socket provided
    SocketParameterMissing,
    /// Underlying QueuePair error
    NetQueuePair(net_util::NetQueuePairError),
    /// Failed registering the TAP listener
    RegisterTapListener(io::Error),
    /// Failed processing control queue
    ProcessCtrlQueue(net_util::CtrlQueueError),
}

pub const SYNTAX: &str = "vhost-user-net backend parameters \
\"ip=<ip_addr>,mask=<net_mask>,socket=<socket_path>,client=on|off,\
num_queues=<number_of_queues>,queue_size=<size_of_each_queue>,tap=<if_name>\"";

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "vhost_user_net_error: {:?}", self)
    }
}

impl std::error::Error for Error {}

impl std::convert::From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        std::io::Error::new(io::ErrorKind::Other, e)
    }
}

struct VhostUserNetThread {
    net: NetQueuePair,
    kill_evt: EventFd,
}

impl VhostUserNetThread {
    /// Create a new virtio network device with the given TAP interface.
    fn new(tap: Tap) -> Result<Self> {
        Ok(VhostUserNetThread {
            kill_evt: EventFd::new(EFD_NONBLOCK).map_err(Error::CreateKillEventFd)?,
            net: NetQueuePair {
                mem: None,
                tap,
                rx: RxVirtio::new(),
                tx: TxVirtio::new(),
                rx_tap_listening: false,
                epoll_fd: None,
                counters: NetCounters::default(),
                tap_event_id: 2,
                rx_desc_avail: false,
                rx_rate_limiter: None,
                tx_rate_limiter: None,
            },
        })
    }

    fn set_vring_worker(&mut self, vring_worker: Option<Arc<VringWorker>>) {
        self.net.epoll_fd = Some(vring_worker.as_ref().unwrap().as_raw_fd());
    }
}

struct VhostUserNetCtrlThread {
    ctrl: CtrlQueue,
    kill_evt: EventFd,
    id: usize,
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
}

impl VhostUserNetCtrlThread {
    fn new(taps: Vec<Tap>, id: usize) -> Result<Self> {
        Ok(VhostUserNetCtrlThread {
            ctrl: CtrlQueue::new(taps),
            kill_evt: EventFd::new(EFD_NONBLOCK).map_err(Error::CreateKillEventFd)?,
            id,
            mem: None,
        })
    }
}

pub struct VhostUserNetBackend {
    threads: Vec<Mutex<VhostUserNetThread>>,
    ctrl_thread: Mutex<VhostUserNetCtrlThread>,
    num_queues: usize,
    queue_size: u16,
    queues_per_thread: Vec<u64>,
}

impl VhostUserNetBackend {
    fn new(
        ip_addr: Ipv4Addr,
        host_mac: MacAddr,
        netmask: Ipv4Addr,
        mut num_queues: usize,
        queue_size: u16,
        ifname: Option<&str>,
    ) -> Result<Self> {
        let taps = open_tap(
            ifname,
            Some(ip_addr),
            Some(netmask),
            &mut Some(host_mac),
            num_queues / 2,
            None,
        )
        .map_err(Error::OpenTap)?;

        let mut queues_per_thread = Vec::new();
        let mut threads = Vec::new();
        for (i, tap) in taps.clone().drain(..).enumerate() {
            let thread = Mutex::new(VhostUserNetThread::new(tap)?);
            threads.push(thread);
            queues_per_thread.push(0b11 << (i * 2));
        }

        // Create a dedicated thread for the control queue.
        let ctrl_thread = Mutex::new(VhostUserNetCtrlThread::new(taps.clone(), taps.len())?);
        queues_per_thread.push(1 << num_queues);

        // Increase num_queues by 1 because of the control queue.
        num_queues += 1;

        Ok(VhostUserNetBackend {
            threads,
            ctrl_thread,
            num_queues,
            queue_size,
            queues_per_thread,
        })
    }

    fn handle_queue(
        &self,
        device_event: u16,
        vrings: &[Arc<RwLock<Vring>>],
        thread_id: usize,
    ) -> VhostUserBackendResult<bool> {
        let mut thread = self.threads[thread_id].lock().unwrap();
        match device_event {
            0 => {
                if !thread.net.rx_tap_listening {
                    net_util::register_listener(
                        thread.net.epoll_fd.unwrap(),
                        thread.net.tap.as_raw_fd(),
                        epoll::Events::EPOLLIN,
                        u64::from(thread.net.tap_event_id),
                    )
                    .map_err(Error::RegisterTapListener)?;
                    thread.net.rx_tap_listening = true;
                }
            }
            1 => {
                let mut vring = vrings[1].write().unwrap();
                if thread
                    .net
                    .process_tx(&mut vring.mut_queue())
                    .map_err(Error::NetQueuePair)?
                {
                    vring
                        .signal_used_queue()
                        .map_err(Error::FailedSignalingUsedQueue)?
                }
            }
            2 => {
                let mut vring = vrings[0].write().unwrap();
                if thread
                    .net
                    .process_rx(&mut vring.mut_queue())
                    .map_err(Error::NetQueuePair)?
                {
                    vring
                        .signal_used_queue()
                        .map_err(Error::FailedSignalingUsedQueue)?
                }
            }
            _ => return Err(Error::HandleEventUnknownEvent.into()),
        }

        Ok(false)
    }

    fn handle_ctrl_queue(
        &self,
        device_event: u16,
        vrings: &[Arc<RwLock<Vring>>],
    ) -> VhostUserBackendResult<bool> {
        let mut thread = self.ctrl_thread.lock().unwrap();

        let mem = thread.mem.as_ref().unwrap().memory();

        match device_event {
            0 => {
                let mut vring = vrings[0].write().unwrap();
                if thread
                    .ctrl
                    .process(&mem, &mut vring.mut_queue())
                    .map_err(Error::ProcessCtrlQueue)?
                {
                    vring
                        .signal_used_queue()
                        .map_err(Error::FailedSignalingUsedQueue)?;
                }
            }
            _ => return Err(Error::HandleEventUnknownEvent.into()),
        }

        Ok(false)
    }
}

impl VhostUserBackend for VhostUserNetBackend {
    fn num_queues(&self) -> usize {
        self.num_queues
    }

    fn max_queue_size(&self) -> usize {
        self.queue_size as usize
    }

    fn features(&self) -> u64 {
        1 << VIRTIO_F_VERSION_1
            | 1 << VIRTIO_RING_F_EVENT_IDX
            | 1 << VIRTIO_NET_F_CTRL_VQ
            | 1 << VIRTIO_NET_F_MQ
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::REPLY_ACK
            | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS
    }

    fn set_event_idx(&mut self, _enabled: bool) {}

    fn update_memory(&mut self, mem: GuestMemoryMmap) -> VhostUserBackendResult<()> {
        for thread in self.threads.iter() {
            thread.lock().unwrap().net.mem = Some(GuestMemoryAtomic::new(mem.clone()));
        }
        self.ctrl_thread.lock().unwrap().mem = Some(GuestMemoryAtomic::new(mem));

        Ok(())
    }

    fn handle_event(
        &self,
        device_event: u16,
        evset: epoll::Events,
        vrings: &[Arc<RwLock<Vring>>],
        thread_id: usize,
    ) -> VhostUserBackendResult<bool> {
        if evset != epoll::Events::EPOLLIN {
            return Err(Error::HandleEventNotEpollIn.into());
        }

        if thread_id == self.ctrl_thread.lock().unwrap().id {
            self.handle_ctrl_queue(device_event, vrings)
        } else {
            self.handle_queue(device_event, vrings, thread_id)
        }
    }

    fn exit_event(&self, thread_index: usize) -> Option<(EventFd, Option<u16>)> {
        let (exit_event_fd, exit_event_index) =
            if thread_index == self.ctrl_thread.lock().unwrap().id {
                // The exit event is placed after the control queue event, which is
                // event index 1.
                (
                    self.ctrl_thread
                        .lock()
                        .unwrap()
                        .kill_evt
                        .try_clone()
                        .unwrap(),
                    1,
                )
            } else {
                // The exit event is placed after the queues and the tap event, which
                // is event index 3.
                (
                    self.threads[thread_index]
                        .lock()
                        .unwrap()
                        .kill_evt
                        .try_clone()
                        .unwrap(),
                    3,
                )
            };

        Some((exit_event_fd, Some(exit_event_index)))
    }

    fn queues_per_thread(&self) -> Vec<u64> {
        self.queues_per_thread.clone()
    }
}

pub struct VhostUserNetBackendConfig {
    pub ip: Ipv4Addr,
    pub host_mac: MacAddr,
    pub mask: Ipv4Addr,
    pub socket: String,
    pub num_queues: usize,
    pub queue_size: u16,
    pub tap: Option<String>,
    pub client: bool,
}

impl VhostUserNetBackendConfig {
    pub fn parse(backend: &str) -> Result<Self> {
        let mut parser = OptionParser::new();

        parser
            .add("tap")
            .add("ip")
            .add("host_mac")
            .add("mask")
            .add("queue_size")
            .add("num_queues")
            .add("socket")
            .add("client");

        parser.parse(backend).map_err(Error::FailedConfigParse)?;

        let tap = parser.get("tap");
        let ip = parser
            .convert("ip")
            .map_err(Error::FailedConfigParse)?
            .unwrap_or_else(|| Ipv4Addr::new(192, 168, 100, 1));
        let host_mac = parser
            .convert("host_mac")
            .map_err(Error::FailedConfigParse)?
            .unwrap_or_else(MacAddr::local_random);
        let mask = parser
            .convert("mask")
            .map_err(Error::FailedConfigParse)?
            .unwrap_or_else(|| Ipv4Addr::new(255, 255, 255, 0));
        let queue_size = parser
            .convert("queue_size")
            .map_err(Error::FailedConfigParse)?
            .unwrap_or(256);
        let num_queues = parser
            .convert("num_queues")
            .map_err(Error::FailedConfigParse)?
            .unwrap_or(2);
        let socket = parser.get("socket").ok_or(Error::SocketParameterMissing)?;
        let client = parser
            .convert::<Toggle>("client")
            .map_err(Error::FailedConfigParse)?
            .unwrap_or(Toggle(false))
            .0;

        Ok(VhostUserNetBackendConfig {
            ip,
            host_mac,
            mask,
            socket,
            num_queues,
            queue_size,
            tap,
            client,
        })
    }
}

pub fn start_net_backend(backend_command: &str) {
    let backend_config = match VhostUserNetBackendConfig::parse(backend_command) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Failed parsing parameters {:?}", e);
            process::exit(1);
        }
    };

    let tap = backend_config.tap.as_deref();

    let net_backend = Arc::new(RwLock::new(
        VhostUserNetBackend::new(
            backend_config.ip,
            backend_config.host_mac,
            backend_config.mask,
            backend_config.num_queues,
            backend_config.queue_size,
            tap,
        )
        .unwrap(),
    ));

    let mut net_daemon =
        VhostUserDaemon::new("vhost-user-net-backend".to_string(), net_backend.clone()).unwrap();

    let mut vring_workers = net_daemon.get_vring_workers();

    if vring_workers.len() != (net_backend.read().unwrap().threads.len() + 1) {
        error!("Number of vring workers must be identical to the number of backend threads");
        process::exit(1);
    }

    for thread in net_backend.read().unwrap().threads.iter() {
        thread
            .lock()
            .unwrap()
            .set_vring_worker(Some(vring_workers.remove(0)));
    }

    if let Err(e) = if backend_config.client {
        net_daemon.start_client(&backend_config.socket)
    } else {
        net_daemon.start_server(Listener::new(&backend_config.socket, true).unwrap())
    } {
        error!(
            "failed to start daemon for vhost-user-net with error: {:?}",
            e
        );
        process::exit(1);
    }

    if let Err(e) = net_daemon.wait() {
        error!("Error from the main thread: {:?}", e);
    }

    for thread in net_backend.read().unwrap().threads.iter() {
        if let Err(e) = thread.lock().unwrap().kill_evt.write(1) {
            error!("Error shutting down worker thread: {:?}", e)
        }
    }
    if let Err(e) = net_backend
        .read()
        .unwrap()
        .ctrl_thread
        .lock()
        .unwrap()
        .kill_evt
        .write(1)
    {
        error!("Error shutting down control thread: {:?}", e);
    };
}
