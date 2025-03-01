// Copyright 2019 Intel Corporation. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::vu_common_ctrl::{
    add_memory_region, reset_vhost_user, setup_vhost_user, update_mem_table,
};
use super::{Error, Result};
use crate::seccomp_filters::{get_seccomp_filter, Thread};
use crate::vhost_user::handler::{VhostUserEpollConfig, VhostUserEpollHandler};
use crate::{
    ActivateError, ActivateResult, Queue, UserspaceMapping, VirtioCommon, VirtioDevice,
    VirtioDeviceType, VirtioInterrupt, VirtioSharedMemoryList, VIRTIO_F_VERSION_1,
};
use libc::{self, c_void, off64_t, pread64, pwrite64};
use seccomp::{SeccompAction, SeccompFilter};
use std::io;
use std::ops::Deref;
use std::os::unix::io::{AsRawFd, RawFd};
use std::result;
use std::sync::{Arc, Barrier};
use std::thread;
use vhost::vhost_user::message::{
    VhostUserFSSlaveMsg, VhostUserFSSlaveMsgFlags, VhostUserProtocolFeatures,
    VhostUserVirtioFeatures, VHOST_USER_FS_SLAVE_ENTRIES,
};
use vhost::vhost_user::{
    HandlerResult, Master, MasterReqHandler, VhostUserMaster, VhostUserMasterReqHandler,
};
use vhost::VhostBackend;
use vm_memory::{
    Address, ByteValued, GuestAddress, GuestAddressSpace, GuestMemory, GuestMemoryAtomic,
    GuestMemoryMmap, GuestRegionMmap, MmapRegion,
};
use vm_migration::{Migratable, MigratableError, Pausable, Snapshottable, Transportable};
use vmm_sys_util::eventfd::EventFd;

const NUM_QUEUE_OFFSET: usize = 1;

struct SlaveReqHandler {
    cache_offset: GuestAddress,
    cache_size: u64,
    mmap_cache_addr: u64,
    mem: GuestMemoryAtomic<GuestMemoryMmap>,
}

impl SlaveReqHandler {
    // Make sure request is within cache range
    fn is_req_valid(&self, offset: u64, len: u64) -> bool {
        let end = match offset.checked_add(len) {
            Some(n) => n,
            None => return false,
        };

        !(offset >= self.cache_size || end > self.cache_size)
    }
}

impl VhostUserMasterReqHandler for SlaveReqHandler {
    fn handle_config_change(&self) -> HandlerResult<u64> {
        debug!("handle_config_change");
        Ok(0)
    }

    fn fs_slave_map(&self, fs: &VhostUserFSSlaveMsg, fd: RawFd) -> HandlerResult<u64> {
        debug!("fs_slave_map");

        for i in 0..VHOST_USER_FS_SLAVE_ENTRIES {
            let offset = fs.cache_offset[i];
            let len = fs.len[i];

            // Ignore if the length is 0.
            if len == 0 {
                continue;
            }

            if !self.is_req_valid(offset, len) {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }

            let addr = self.mmap_cache_addr + offset;
            let flags = fs.flags[i];
            let ret = unsafe {
                libc::mmap(
                    addr as *mut libc::c_void,
                    len as usize,
                    flags.bits() as i32,
                    libc::MAP_SHARED | libc::MAP_FIXED,
                    fd,
                    fs.fd_offset[i] as libc::off_t,
                )
            };
            if ret == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }

            let ret = unsafe { libc::close(fd) };
            if ret == -1 {
                return Err(io::Error::last_os_error());
            }
        }

        Ok(0)
    }

    fn fs_slave_unmap(&self, fs: &VhostUserFSSlaveMsg) -> HandlerResult<u64> {
        debug!("fs_slave_unmap");

        for i in 0..VHOST_USER_FS_SLAVE_ENTRIES {
            let offset = fs.cache_offset[i];
            let mut len = fs.len[i];

            // Ignore if the length is 0.
            if len == 0 {
                continue;
            }

            // Need to handle a special case where the slave ask for the unmapping
            // of the entire mapping.
            if len == 0xffff_ffff_ffff_ffff {
                len = self.cache_size;
            }

            if !self.is_req_valid(offset, len) {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }

            let addr = self.mmap_cache_addr + offset;
            let ret = unsafe {
                libc::mmap(
                    addr as *mut libc::c_void,
                    len as usize,
                    libc::PROT_NONE,
                    libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_FIXED,
                    -1,
                    0,
                )
            };
            if ret == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
        }

        Ok(0)
    }

    fn fs_slave_sync(&self, fs: &VhostUserFSSlaveMsg) -> HandlerResult<u64> {
        debug!("fs_slave_sync");

        for i in 0..VHOST_USER_FS_SLAVE_ENTRIES {
            let offset = fs.cache_offset[i];
            let len = fs.len[i];

            // Ignore if the length is 0.
            if len == 0 {
                continue;
            }

            if !self.is_req_valid(offset, len) {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }

            let addr = self.mmap_cache_addr + offset;
            let ret =
                unsafe { libc::msync(addr as *mut libc::c_void, len as usize, libc::MS_SYNC) };
            if ret == -1 {
                return Err(io::Error::last_os_error());
            }
        }

        Ok(0)
    }

    fn fs_slave_io(&self, fs: &VhostUserFSSlaveMsg, fd: RawFd) -> HandlerResult<u64> {
        debug!("fs_slave_io");

        let mut done: u64 = 0;
        for i in 0..VHOST_USER_FS_SLAVE_ENTRIES {
            // Ignore if the length is 0.
            if fs.len[i] == 0 {
                continue;
            }

            let mut foffset = fs.fd_offset[i];
            let mut len = fs.len[i] as usize;
            let gpa = fs.cache_offset[i];
            let cache_end = self.cache_offset.raw_value() + self.cache_size;
            let efault = libc::EFAULT;

            let mut ptr = if gpa >= self.cache_offset.raw_value() && gpa < cache_end {
                let offset = gpa
                    .checked_sub(self.cache_offset.raw_value())
                    .ok_or_else(|| io::Error::from_raw_os_error(efault))?;
                let end = gpa
                    .checked_add(fs.len[i])
                    .ok_or_else(|| io::Error::from_raw_os_error(efault))?;

                if end >= cache_end {
                    return Err(io::Error::from_raw_os_error(efault));
                }

                self.mmap_cache_addr + offset
            } else {
                self.mem
                    .memory()
                    .get_host_address(GuestAddress(gpa))
                    .map_err(|e| {
                        error!(
                            "Failed to find RAM region associated with guest physical address 0x{:x}: {:?}",
                            gpa, e
                        );
                        io::Error::from_raw_os_error(efault)
                    })? as u64
            };

            while len > 0 {
                let ret = if (fs.flags[i] & VhostUserFSSlaveMsgFlags::MAP_W)
                    == VhostUserFSSlaveMsgFlags::MAP_W
                {
                    debug!("write: foffset={}, len={}", foffset, len);
                    unsafe { pwrite64(fd, ptr as *const c_void, len as usize, foffset as off64_t) }
                } else {
                    debug!("read: foffset={}, len={}", foffset, len);
                    unsafe { pread64(fd, ptr as *mut c_void, len as usize, foffset as off64_t) }
                };

                if ret < 0 {
                    return Err(io::Error::last_os_error());
                }

                if ret == 0 {
                    // EOF
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "failed to access whole buffer",
                    ));
                }
                len -= ret as usize;
                foffset += ret as u64;
                ptr += ret as u64;
                done += ret as u64;
            }
        }

        let ret = unsafe { libc::close(fd) };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(done)
    }
}

#[derive(Copy, Clone)]
#[repr(C, packed)]
struct VirtioFsConfig {
    tag: [u8; 36],
    num_request_queues: u32,
}

impl Default for VirtioFsConfig {
    fn default() -> Self {
        VirtioFsConfig {
            tag: [0; 36],
            num_request_queues: 0,
        }
    }
}

unsafe impl ByteValued for VirtioFsConfig {}

pub struct Fs {
    common: VirtioCommon,
    id: String,
    vu: Master,
    config: VirtioFsConfig,
    // Hold ownership of the memory that is allocated for the device
    // which will be automatically dropped when the device is dropped
    cache: Option<(VirtioSharedMemoryList, MmapRegion)>,
    slave_req_support: bool,
    seccomp_action: SeccompAction,
    guest_memory: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    acked_protocol_features: u64,
}

impl Fs {
    /// Create a new virtio-fs device.
    pub fn new(
        id: String,
        path: &str,
        tag: &str,
        req_num_queues: usize,
        queue_size: u16,
        cache: Option<(VirtioSharedMemoryList, MmapRegion)>,
        seccomp_action: SeccompAction,
    ) -> Result<Fs> {
        let mut slave_req_support = false;

        // Calculate the actual number of queues needed.
        let num_queues = NUM_QUEUE_OFFSET + req_num_queues;

        // Connect to the vhost-user socket.
        let mut master =
            Master::connect(path, num_queues as u64).map_err(Error::VhostUserCreateMaster)?;

        // Filling device and vring features VMM supports.
        let mut avail_features =
            1 << VIRTIO_F_VERSION_1 | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();

        // Set vhost-user owner.
        master.set_owner().map_err(Error::VhostUserSetOwner)?;

        // Get features from backend, do negotiation to get a feature collection which
        // both VMM and backend support.
        let backend_features = master.get_features().map_err(Error::VhostUserGetFeatures)?;
        avail_features &= backend_features;
        // Set features back is required by the vhost crate mechanism, since the
        // later vhost call will check if features is filled in master before execution.
        master
            .set_features(avail_features)
            .map_err(Error::VhostUserSetFeatures)?;

        // Identify if protocol features are supported by the slave.
        let mut acked_features = 0;
        let mut acked_protocol_features = 0;
        if avail_features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits() != 0 {
            acked_features |= VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();

            let mut protocol_features = master
                .get_protocol_features()
                .map_err(Error::VhostUserGetProtocolFeatures)?;

            let mut supported_protocol_features = VhostUserProtocolFeatures::MQ
                | VhostUserProtocolFeatures::REPLY_ACK
                | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS;

            if cache.is_some() {
                supported_protocol_features |=
                    VhostUserProtocolFeatures::SLAVE_REQ | VhostUserProtocolFeatures::SLAVE_SEND_FD
            }

            protocol_features &= supported_protocol_features;

            master
                .set_protocol_features(protocol_features)
                .map_err(Error::VhostUserSetProtocolFeatures)?;

            acked_protocol_features = protocol_features.bits();

            slave_req_support = true;
        }

        // Create virtio-fs device configuration.
        let mut config = VirtioFsConfig::default();
        let tag_bytes_vec = tag.to_string().into_bytes();
        config.tag[..tag_bytes_vec.len()].copy_from_slice(tag_bytes_vec.as_slice());
        config.num_request_queues = req_num_queues as u32;

        Ok(Fs {
            common: VirtioCommon {
                device_type: VirtioDeviceType::Fs as u32,
                avail_features,
                acked_features,
                queue_sizes: vec![queue_size; num_queues],
                paused_sync: Some(Arc::new(Barrier::new(2))),
                min_queues: NUM_QUEUE_OFFSET as u16,
                ..Default::default()
            },
            id,
            vu: master,
            config,
            cache,
            slave_req_support,
            seccomp_action,
            guest_memory: None,
            acked_protocol_features,
        })
    }
}

impl Drop for Fs {
    fn drop(&mut self) {
        if let Some(kill_evt) = self.common.kill_evt.take() {
            // Ignore the result because there is nothing we can do about it.
            let _ = kill_evt.write(1);
        }
    }
}

impl VirtioDevice for Fs {
    fn device_type(&self) -> u32 {
        self.common.device_type
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.common.queue_sizes
    }

    fn features(&self) -> u64 {
        self.common.avail_features
    }

    fn ack_features(&mut self, value: u64) {
        self.common.ack_features(value)
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        self.read_config_from_slice(self.config.as_slice(), offset, data);
    }

    fn activate(
        &mut self,
        mem: GuestMemoryAtomic<GuestMemoryMmap>,
        interrupt_cb: Arc<dyn VirtioInterrupt>,
        queues: Vec<Queue>,
        queue_evts: Vec<EventFd>,
    ) -> ActivateResult {
        self.common.activate(&queues, &queue_evts, &interrupt_cb)?;

        self.guest_memory = Some(mem.clone());

        let kill_evt = self
            .common
            .kill_evt
            .as_ref()
            .unwrap()
            .try_clone()
            .map_err(|e| {
                error!("failed to clone kill_evt eventfd: {}", e);
                ActivateError::BadActivate
            })?;
        let pause_evt = self
            .common
            .pause_evt
            .as_ref()
            .unwrap()
            .try_clone()
            .map_err(|e| {
                error!("failed to clone pause_evt eventfd: {}", e);
                ActivateError::BadActivate
            })?;

        setup_vhost_user(
            &mut self.vu,
            &mem.memory(),
            queues,
            queue_evts,
            &interrupt_cb,
        )
        .map_err(ActivateError::VhostUserFsSetup)?;

        // Initialize slave communication.
        let slave_req_handler = if self.slave_req_support {
            if let Some(cache) = self.cache.as_ref() {
                let vu_master_req_handler = Arc::new(SlaveReqHandler {
                    cache_offset: cache.0.addr,
                    cache_size: cache.0.len,
                    mmap_cache_addr: cache.0.host_addr,
                    mem,
                });

                let mut req_handler =
                    MasterReqHandler::new(vu_master_req_handler).map_err(|e| {
                        ActivateError::VhostUserFsSetup(Error::MasterReqHandlerCreation(e))
                    })?;
                req_handler.set_reply_ack_flag(true);
                self.vu
                    .set_slave_request_fd(req_handler.get_tx_raw_fd())
                    .map_err(|e| {
                        ActivateError::VhostUserFsSetup(Error::VhostUserSetSlaveRequestFd(e))
                    })?;
                Some(req_handler)
            } else {
                None
            }
        } else {
            None
        };

        let mut handler = VhostUserEpollHandler::new(VhostUserEpollConfig {
            kill_evt,
            pause_evt,
            slave_req_handler,
        });

        let paused = self.common.paused.clone();
        let paused_sync = self.common.paused_sync.clone();
        let mut epoll_threads = Vec::new();
        let virtio_vhost_fs_seccomp_filter =
            get_seccomp_filter(&self.seccomp_action, Thread::VirtioVhostFs)
                .map_err(ActivateError::CreateSeccompFilter)?;
        thread::Builder::new()
            .name(self.id.clone())
            .spawn(move || {
                if let Err(e) = SeccompFilter::apply(virtio_vhost_fs_seccomp_filter) {
                    error!("Error applying seccomp filter: {:?}", e);
                } else if let Err(e) = handler.run(paused, paused_sync.unwrap()) {
                    error!("Error running worker: {:?}", e);
                }
            })
            .map(|thread| epoll_threads.push(thread))
            .map_err(|e| {
                error!("failed to clone queue EventFd: {}", e);
                ActivateError::BadActivate
            })?;

        self.common.epoll_threads = Some(epoll_threads);

        event!("virtio-device", "activated", "id", &self.id);
        Ok(())
    }

    fn reset(&mut self) -> Option<Arc<dyn VirtioInterrupt>> {
        // We first must resume the virtio thread if it was paused.
        if self.common.pause_evt.take().is_some() {
            self.common.resume().ok()?;
        }

        if let Err(e) = reset_vhost_user(&mut self.vu, self.common.queue_sizes.len()) {
            error!("Failed to reset vhost-user daemon: {:?}", e);
            return None;
        }

        if let Some(kill_evt) = self.common.kill_evt.take() {
            // Ignore the result because there is nothing we can do about it.
            let _ = kill_evt.write(1);
        }

        event!("virtio-device", "reset", "id", &self.id);

        // Return the interrupt
        Some(self.common.interrupt_cb.take().unwrap())
    }

    fn shutdown(&mut self) {
        let _ = unsafe { libc::close(self.vu.as_raw_fd()) };
    }

    fn get_shm_regions(&self) -> Option<VirtioSharedMemoryList> {
        self.cache.as_ref().map(|cache| cache.0.clone())
    }

    fn set_shm_regions(
        &mut self,
        shm_regions: VirtioSharedMemoryList,
    ) -> std::result::Result<(), crate::Error> {
        if let Some(mut cache) = self.cache.as_mut() {
            cache.0 = shm_regions;
            Ok(())
        } else {
            Err(crate::Error::SetShmRegionsNotSupported)
        }
    }

    fn add_memory_region(
        &mut self,
        region: &Arc<GuestRegionMmap>,
    ) -> std::result::Result<(), crate::Error> {
        if self.acked_protocol_features & VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS.bits() != 0
        {
            add_memory_region(&mut self.vu, region).map_err(crate::Error::VhostUserAddMemoryRegion)
        } else if let Some(guest_memory) = &self.guest_memory {
            update_mem_table(&mut self.vu, guest_memory.memory().deref())
                .map_err(crate::Error::VhostUserUpdateMemory)
        } else {
            Ok(())
        }
    }

    fn userspace_mappings(&self) -> Vec<UserspaceMapping> {
        let mut mappings = Vec::new();
        if let Some(cache) = self.cache.as_ref() {
            mappings.push(UserspaceMapping {
                host_addr: cache.0.host_addr,
                mem_slot: cache.0.mem_slot,
                addr: cache.0.addr,
                len: cache.0.len,
                mergeable: false,
            })
        }

        mappings
    }
}

impl Pausable for Fs {
    fn pause(&mut self) -> result::Result<(), MigratableError> {
        self.common.pause()
    }

    fn resume(&mut self) -> result::Result<(), MigratableError> {
        self.common.resume()
    }
}

impl Snapshottable for Fs {
    fn id(&self) -> String {
        self.id.clone()
    }
}
impl Transportable for Fs {}
impl Migratable for Fs {}
