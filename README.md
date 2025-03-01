[![Build Status](https://travis-ci.com/cloud-hypervisor/cloud-hypervisor.svg?branch=master)](https://travis-ci.com/cloud-hypervisor/cloud-hypervisor)

- [1. What is Cloud Hypervisor?](#1-what-is-cloud-hypervisor)
  - [Objectives](#objectives)
    - [High Level](#high-level)
    - [Architectures](#architectures)
    - [Guest OS](#guest-os)
- [2. Getting Started](#2-getting-started)
  - [Clone and build](#clone-and-build)
    - [Containerized builds and tests](#containerized-builds-and-tests)
  - [Run](#run)
    - [Cloud image](#cloud-image)
    - [Custom kernel and disk image](#custom-kernel-and-disk-image)
      - [Building your kernel](#building-your-kernel)
      - [Disk image](#disk-image)
      - [Booting the guest VM](#booting-the-guest-vm)
- [3. Status](#3-status)
  - [Hot Plug](#hot-plug)
  - [Device Model](#device-model)
  - [TODO](#todo)
- [4. `rust-vmm` project dependency](#4-rust-vmm-project-dependency)
  - [Firecracker and crosvm](#firecracker-and-crosvm)
- [5. Community](#5-community)
  - [Contribute](#contribute)
  - [Join us](#join-us)

# 1. What is Cloud Hypervisor?

Cloud Hypervisor is an open source Virtual Machine Monitor (VMM) that runs on top of [KVM](https://www.kernel.org/doc/Documentation/virtual/kvm/api.txt) and the MSHV hypervisors .

The project focuses on exclusively running modern, cloud workloads, on top of a limited set of hardware architectures and platforms.
Cloud workloads refers to those that are usually run by customers inside a cloud provider. For our purposes this means modern operating systems with most I/O handled by paravirtualised devices (i.e. virtio), no requirement for legacy devices, and 64-bit CPUs.

Cloud Hypervisor is implemented in [Rust](https://www.rust-lang.org/) and is based on the [rust-vmm](https://github.com/rust-vmm) crates.

## Objectives

### High Level

- Runs on KVM or MSHV
- Minimal emulation
- Low latency
- Low memory footprint
- Low complexity
- High performance
- Small attack surface
- 64-bit support only
- CPU, memory, PCI hotplug
- Machine to machine migration

### Architectures

Cloud Hypervisor supports the `x86-64` and `AArch64` architectures. There are some small differences in functionality between the two architectures (see [#1125](https://github.com/cloud-hypervisor/cloud-hypervisor/issues/1125)).

### Guest OS

Cloud Hypervisor supports `64-bit Linux` and Windows 10/Windows Server 2019.

# 2. Getting Started

We create a folder to build and run `cloud-hypervisor` at `$HOME/cloud-hypervisor`

```shell
$ export CLOUDH=$HOME/cloud-hypervisor
$ mkdir $CLOUDH
```

## Install prerequisites

You need to install some prerequisite packages in order to build and test Cloud Hypervisor.
Here, all the steps are based on Ubuntu, for other Linux distributions please replace the
package manager and package name.

```shell
# Install git
$ sudo apt install git
# Install rust tool chain
$ curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# Install build-essential
$ sudo apt install build-essential
# If you want to build statically linked binary please add musl target
$ rustup target add x86_64-unknown-linux-musl
```

## Clone and build

First you need to clone and build the cloud-hypervisor repo:

```shell
$ pushd $CLOUDH
$ git clone https://github.com/cloud-hypervisor/cloud-hypervisor.git
$ cd cloud-hypervisor
$ cargo build --release

# We need to give the cloud-hypervisor binary the NET_ADMIN capabilities for it to set TAP interfaces up on the host.
$ sudo setcap cap_net_admin+ep ./target/release/cloud-hypervisor

# If you want to build statically linked binary
$ cargo build --release --target=x86_64-unknown-linux-musl --all
$ popd
```

This will build a `cloud-hypervisor` binary under `$CLOUDH/cloud-hypervisor/target/release/cloud-hypervisor`.

### Containerized builds and tests

If you want to build and test Cloud Hypervisor without having to install all the
required dependencies (The rust toolchain, cargo tools, etc), you can also use
Cloud Hypervisor's development script: `dev_cli.sh`. Please note that upon its
first invocation, this script will pull a fairly large container image.

For example, to build the Cloud Hypervisor release binary:

```shell
$ pushd $CLOUDH
$ cd cloud-hypervisor
$ ./scripts/dev_cli.sh build --release
```

With `dev_cli.sh`, one can also run the Cloud Hypervisor CI locally. This can be
very convenient for debugging CI errors without having to fully rely on the
Cloud Hypervisor CI infrastructure.

For example, to run the Cloud Hypervisor unit tests:

```shell
$ ./scripts/dev_cli.sh tests --unit
```

Run the `./scripts/dev_cli.sh --help` command to view all the supported
development script commands and their related options.

## Run

You can run a guest VM by either using an existing cloud image or booting into your own kernel and disk image.

### Cloud image

Cloud Hypervisor supports booting disk images containing all needed
components to run cloud workloads, a.k.a. cloud images. To do that we rely on
the [Rust Hypervisor
Firmware](https://github.com/cloud-hypervisor/rust-hypervisor-firmware) project to provide
an ELF
formatted KVM firmware for `cloud-hypervisor` to directly boot into.

We need to get the latest `rust-hypervisor-firmware` release and also a working cloud image. Here we will use a Ubuntu image:

```shell
$ pushd $CLOUDH
$ wget https://cloud-images.ubuntu.com/focal/current/focal-server-cloudimg-amd64.img
$ qemu-img convert -p -f qcow2 -O raw focal-server-cloudimg-amd64.img focal-server-cloudimg-amd64.raw
$ wget https://github.com/cloud-hypervisor/rust-hypervisor-firmware/releases/download/0.3.1/hypervisor-fw
$ popd
```

```shell
$ pushd $CLOUDH
$ sudo setcap cap_net_admin+ep ./cloud-hypervisor/target/release/cloud-hypervisor
$ ./cloud-hypervisor/target/release/cloud-hypervisor \
	--kernel ./hypervisor-fw \
	--disk path=focal-server-cloudimg-amd64.raw \
	--cpus boot=4 \
	--memory size=1024M \
	--net "tap=,mac=,ip=,mask=" \
	--rng
$ popd
```

Multiple arguments can be given to the `--disk` parameter.

### Custom kernel and disk image

#### Building your kernel

Cloud Hypervisor also supports direct kernel boot into a `vmlinux` ELF kernel. In order to support virtio-iommu we have our own development branch. You are of course able to use your own kernel but these instructions will continue with the version that we develop and test against.

To build the kernel:

```shell

# Clone the Cloud Hypervisor Linux branch
$ pushd $CLOUDH
$ git clone --depth 1 https://github.com/cloud-hypervisor/linux.git -b ch-5.12 linux-cloud-hypervisor
$ pushd linux-cloud-hypervisor

# Use the cloud-hypervisor kernel config to build your kernel
$ cp $CLOUDH/cloud-hypervisor/resources/linux-config-x86_64 .config
$ make bzImage -j `nproc`
$ popd
```

The `vmlinux` kernel image will then be located at `linux-cloud-hypervisor/arch/x86/boot/compressed/vmlinux.bin`.

#### Disk image

For the disk image, we will use a Ubuntu cloud image that contains a root partition:

```shell
$ pushd $CLOUDH
$ wget https://cloud-images.ubuntu.com/focal/current/focal-server-cloudimg-amd64.img
$ qemu-img convert -p -f qcow2 -O raw focal-server-cloudimg-amd64.img focal-server-cloudimg-amd64.raw
$ popd
```

#### Booting the guest VM

Now we can directly boot into our custom kernel and make it use the Ubuntu root partition.
If we want to have 4 vCPUs and 1024 MBytes of memory:

```shell
$ pushd $CLOUDH
$ sudo setcap cap_net_admin+ep ./cloud-hypervisor/target/release/cloud-hypervisor
$ ./cloud-hypervisor/target/release/cloud-hypervisor \
	--kernel ./linux-cloud-hypervisor/arch/x86/boot/compressed/vmlinux.bin \
	--disk path=focal-server-cloudimg-amd64.raw \
	--cmdline "console=hvc0 root=/dev/vda1 rw" \
	--cpus boot=4 \
	--memory size=1024M \
	--net "tap=,mac=,ip=,mask=" \
	--rng
```

The above example use the `virtio-console` device as the guest console, and this
device may not be enabled soon enough by the guest kernel to get early kernel
debug messages.

When in need for earlier debug messages, using the legacy serial device based
console is preferred:

```
$ ./cloud-hypervisor/target/release/cloud-hypervisor \
	--kernel ./linux-cloud-hypervisor/arch/x86/boot/compressed/vmlinux.bin \
	--console off \
	--serial tty \
	--disk path=focal-server-cloudimg-amd64.raw \
	--cmdline "console=ttyS0 root=/dev/vda1 rw" \
	--cpus boot=4 \
	--memory size=1024M \
	--net "tap=,mac=,ip=,mask=" \
	--rng
```

# 3. Status

Cloud Hypervisor is under active development. The following stability guarantees are currently made:

* The API (including command line options) will not be removed or changed in a
  breaking way without a minimum of 2 releases notice. Where possible warnings
  will be given about the use of deprecated functionality and the deprecations
  will be documented in the release notes.
* Point releases will be made between individual releases where there are
  substantial bug fixes or security issues that need to be fixed.

Currently the following items are **not** guaranteed across updates:

* Snapshot/restore is not supported across different versions
* Live migration is not supported across different versions
* The following features are considered experimental and may change
  substantially between releases: TDX, SGX.


As of 2021-04-29, the following cloud images are supported:

- [Ubuntu Bionic](https://cloud-images.ubuntu.com/bionic/current/) (cloudimg)
- [Ubuntu Focal](https://cloud-images.ubuntu.com/focal/current/) (cloudimg)
- [Ubuntu Groovy](https://cloud-images.ubuntu.com/groovy/current/) (cloudimg)
- [Ubuntu Hirsute](https://cloud-images.ubuntu.com/hirsute/current/) (cloudimg)

Direct kernel boot to userspace should work with a rootfs from most distributions.

## Hot Plug

Cloud Hypervisor supports hotplug of CPUs, passthrough devices (VFIO), `virtio-{net,block,pmem,fs,vsock}` and memory resizing. This [document](docs/hotplug.md) details how to add devices to
a running VM.

## Device Model

Details of the device model can be found in this [documentation](docs/device_model.md).

## TODO

We are not tracking the Cloud Hypervisor TODO list from a specific git tracked file but through
[github issues](https://github.com/cloud-hypervisor/cloud-hypervisor/issues/new) instead.

# 4. `rust-vmm` project dependency

In order to satisfy the design goal of having a high-performance, security-focused hypervisor the decision
was made to use the [Rust](https://www.rust-lang.org/) programming language.
The language's strong focus on memory and thread safety makes it an ideal candidate for implementing VMMs.

Instead of implementing the VMM components from scratch, Cloud Hypervisor is importing the [rust-vmm](https://github.com/rust-vmm)
crates, and sharing code and architecture together with other VMMs like e.g. Amazon's [Firecracker](https://firecracker-microvm.github.io/)
and Google's [crosvm](https://chromium.googlesource.com/chromiumos/platform/crosvm/).

Cloud Hypervisor embraces the rust-vmm project goals, which is to be able to share and re-use
as many virtualization crates as possible. As such, the Cloud Hypervisor relationship with the rust-vmm
project is twofold:

1. It will use as much of the rust-vmm code as possible. Any new rust-vmm crate that's relevant to the project
   goals will be integrated as soon as possible.
2. As it is likely that the rust-vmm project will lack some of the features that Cloud Hypervisor needs (e.g. ACPI,
   VFIO, vhost-user, etc), we will be using the Cloud Hypervisor VMM to implement and test them, and contribute them
   back to the rust-vmm project.

## Firecracker and crosvm

A large part of the Cloud Hypervisor code is based on either the Firecracker or the crosvm projects implementations.
Both of these are VMMs written in Rust with a focus on safety and security, like Cloud Hypervisor.

However we want to emphasize that the Cloud Hypervisor project is neither a fork nor a reimplementation of any of those
projects. The goals and use cases we're trying to meet are different.
We're aiming at supporting cloud workloads, i.e. those modern, full Linux distribution images currently being run by
Cloud Service Provider (CSP) tenants.

Our primary target is not to support client or serverless use cases, and as such our code base already diverges
from the crosvm and Firecracker ones. As we add more features to support our use cases, we believe that the divergence
will increase while at the same time sharing as much of the fundamental virtualization code through the rust-vmm project
crates as possible.

# 5. Community

The Cloud Hypervisor project follows the governance, and community guidelines described in
the [Community](https://github.com/cloud-hypervisor/community) repository.

## Contribute

We are working on building a global, diverse and collaborative community around the Cloud Hypervisor project.
Anyone who is interested in [contributing](CONTRIBUTING.md) to the project is welcome to participate.

We believe that contributing to a open source project like Cloud Hypervisor covers a lot more than just sending
code. Testing, documentation, pull request reviews, bug reports, feature requests, project improvement suggestions,
etc, are all equal and welcome means of contribution. See the [CONTRIBUTING](CONTRIBUTING.md) document for more details.

## Join us

Get an [invite to our Slack channel](https://join.slack.com/t/cloud-hypervisor/shared_invite/enQtNjY3MTE3MDkwNDQ4LWQ1MTA1ZDVmODkwMWQ1MTRhYzk4ZGNlN2UwNTI3ZmFlODU0OTcwOWZjMTkwZDExYWE3YjFmNzgzY2FmNDAyMjI)
and [join us on Slack](https://cloud-hypervisor.slack.com/).

## Security issues

Please use the GitHub security advisories feature for reporting issues: https://github.com/cloud-hypervisor/cloud-hypervisor/security/advisories/new
