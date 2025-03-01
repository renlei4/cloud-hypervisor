// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause
//
// Copyright © 2020, Microsoft Corporation
//
// Copyright 2018-2019 CrowdStrike, Inc.
//
//
use crate::vm::Vm;
#[cfg(target_arch = "x86_64")]
use crate::x86_64::CpuId;
#[cfg(target_arch = "x86_64")]
use crate::x86_64::MsrList;
use std::sync::Arc;
use thiserror::Error;

#[derive(Error, Debug)]
///
///
pub enum HypervisorError {
    ///
    /// hypervisor creation error
    ///
    #[error("Failed to create the hypervisor: {0}")]
    HypervisorCreate(#[source] anyhow::Error),
    ///
    /// Vm creation failure
    ///
    #[error("Failed to create Vm: {0}")]
    VmCreate(#[source] anyhow::Error),
    ///
    /// Vm setup failure
    ///
    #[error("Failed to setup Vm: {0}")]
    VmSetup(#[source] anyhow::Error),
    ///
    /// API version error
    ///
    #[error("Failed to get API Version: {0}")]
    GetApiVersion(#[source] anyhow::Error),
    ///
    /// CpuId error
    ///
    #[error("Failed to get cpuid: {0}")]
    GetCpuId(#[source] anyhow::Error),
    ///
    /// Failed to retrieve list of MSRs.
    ///
    #[error("Failed to get the list of supported MSRs: {0}")]
    GetMsrList(#[source] anyhow::Error),
    ///
    /// API version is not compatible
    ///
    #[error("Incompatible API version")]
    IncompatibleApiVersion,
    ///
    /// Checking extensions failed
    ///
    #[error("Checking extensions:{0}")]
    CheckExtensions(#[source] anyhow::Error),
}

///
/// Result type for returning from a function
///
pub type Result<T> = std::result::Result<T, HypervisorError>;

///
/// Trait to represent a Hypervisor
///
/// This crate provides a hypervisor-agnostic interfaces
///
pub trait Hypervisor: Send + Sync {
    ///
    /// Create a Vm using the underlying hypervisor
    /// Return a hypervisor-agnostic Vm trait object
    ///
    fn create_vm(&self) -> Result<Arc<dyn Vm>>;
    ///
    /// Create a Vm of a specific type using the underlying hypervisor
    /// Return a hypervisor-agnostic Vm trait object
    ///
    fn create_vm_with_type(&self, _vm_type: u64) -> Result<Arc<dyn Vm>> {
        unreachable!()
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// Get the supported CpuID
    ///
    fn get_cpuid(&self) -> Result<CpuId>;
    ///
    /// Check particular extensions if any
    ///
    fn check_required_extensions(&self) -> Result<()> {
        Ok(())
    }
    #[cfg(target_arch = "x86_64")]
    ///
    /// Retrieve the list of MSRs supported by the hypervisor.
    ///
    fn get_msr_list(&self) -> Result<MsrList>;
}
