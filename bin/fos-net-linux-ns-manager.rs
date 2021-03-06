/*********************************************************************************
* Copyright (c) 2018,2020 ADLINK Technology Inc.
*
* This program and the accompanying materials are made available under the
* terms of the Eclipse Public License 2.0 which is available at
* http://www.eclipse.org/legal/epl-2.0, or the Apache Software License 2.0
* which is available at https://www.apache.org/licenses/LICENSE-2.0.
*
* SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
* Contributors:
*   ADLINK fog05 team, <fog05@adlink-labs.tech>
*********************************************************************************/
#![allow(unused)]
#![feature(async_closure)]

use std::collections::HashMap;
use std::path::Path;
use std::process;
use std::process::Command;
use std::str;
use std::time::Duration;

use async_std::fs;
use async_std::path::Path as AsyncPath;
use async_std::prelude::*;
use async_std::sync::{Arc, RwLock};
use async_std::task;

use futures::stream::TryStreamExt;

use zenoh::*;

use fog05_sdk::fresult::{FError, FResult};
use fog05_sdk::types::IPAddress;
use fog05_sdk::zconnector::ZConnector;

use znrpc_macros::znserver;
use zrpc::ZNServe;

use signal_hook_async_std::Signals;

use uuid::Uuid;

use structopt::StructOpt;

use git_version::git_version;

use fog05_networking_linux::types::NamespaceManager;

use netlink_packet_route::rtnl::address::nlas::Nla;
use rtnetlink::new_connection;
use rtnetlink::packet::rtnl::link::nlas::Nla as LinkNla;

use ipnetwork::IpNetwork;

use nix::fcntl::OFlag;
use nix::sched::CloneFlags;
use nix::sys::stat::Mode;

const NETNS_PATH: &str = "/run/netns/";
pub const NONE_FS: &str = "none";
pub const SYS_FS: &str = "sysfs";

const GIT_VERSION: &str = git_version!(prefix = "v", cargo_prefix = "v");

#[derive(StructOpt, Debug)]
struct NSManagerArgs {
    /// Config file
    #[structopt(short, long)]
    netns: String,
    #[structopt(short, long)]
    locator: String,
    #[structopt(short, long)]
    id: Uuid,
}

pub struct NSManagerState {
    pub tokio_rt: tokio::runtime::Runtime,
    pub nl_handler: rtnetlink::Handle,
}

#[derive(Clone)]
pub struct NSManager {
    pub z: Arc<zenoh::net::Session>,
    pub pid: u32,
    pub uuid: Uuid,
    pub state: Arc<RwLock<NSManagerState>>,
}

fn main() {
    // Init logging
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "trace"),
    );
    let args = NSManagerArgs::from_args();

    log::debug!(
        "Eclipse fog05 Linux Networking plugin Namespace manager {}",
        GIT_VERSION
    );
    log::trace!("Args: {:?}", args);

    log::trace!("Changing namespace");
    // https://github.com/shemminger/iproute2/blob/f33a871b8094ae0f6e6293804e1cc6edbba0e108/lib/namespace.c#L49
    let mut unshare_flags = CloneFlags::empty();
    let mut setns_flags = CloneFlags::empty();
    let mut open_flags = OFlag::empty();
    let mut mount_flags = nix::mount::MsFlags::empty();
    let none_p4: Option<&Path> = None;

    unshare_flags.insert(CloneFlags::CLONE_NEWNS);

    let mut netns_path = String::new();
    netns_path.push_str(NETNS_PATH);
    netns_path.push_str(&args.netns);

    open_flags.insert(OFlag::O_RDONLY);
    open_flags.insert(OFlag::O_CLOEXEC);

    let fd = match nix::fcntl::open(Path::new(&netns_path), open_flags, Mode::empty()) {
        Ok(raw_fd) => raw_fd,
        Err(e) => {
            log::error!("open error {}", e);
            process::exit(-1);
        }
    };

    setns_flags.insert(CloneFlags::CLONE_NEWNET);
    match nix::sched::setns(fd, setns_flags) {
        Err(e) => {
            let _ = nix::unistd::close(fd);
            log::error!("setns error {}", e);
            process::exit(-1);
        }
        Ok(_) => {
            let _ = nix::unistd::close(fd);

            if let Err(e) = nix::sched::unshare(unshare_flags) {
                log::error!("Unshare error {}", e);
                process::exit(-1);
            }

            let none_fs = Path::new(&NONE_FS);
            mount_flags.insert(nix::mount::MsFlags::MS_REC);
            mount_flags.insert(nix::mount::MsFlags::MS_SLAVE);
            if let Err(e) = nix::mount::mount(
                Some(Path::new("")),
                Path::new("/"),
                Some(none_fs),
                mount_flags,
                none_p4,
            ) {
                log::error!("mount error {}", e);
                process::exit(-1);
            }

            if let Err(e) = nix::mount::umount2(Path::new("/sys"), nix::mount::MntFlags::MNT_DETACH)
            {
                log::error!("umount2 error {}", e);
                process::exit(-1);
            }

            let sys_fs = Path::new(&SYS_FS);
            mount_flags = nix::mount::MsFlags::empty();
            if let Err(e) = nix::mount::mount(
                Some(Path::new(&args.netns)),
                Path::new("/sys"),
                Some(sys_fs),
                mount_flags,
                none_p4,
            ) {
                log::error!("mount sysfs error {}", e);
                process::exit(-1);
            }

            async fn __main(args: NSManagerArgs) {
                log::info!("Running on namespace {}", args.netns);
                let my_pid = process::id();
                log::trace!("Creating Tokio runtime");
                let rt = tokio::runtime::Runtime::new().unwrap();

                let properties = format!("mode=client;peer={}", args.locator.clone());
                let zproperties = Properties::from(properties);
                let zenoh = Arc::new(zenoh::net::open(zproperties.into()).await.unwrap());

                let mut manager = match NSManager::new(zenoh, my_pid, args.id, rt).await {
                    Ok(m) => m,
                    Err(e) => {
                        log::error!("Error when creating manager: {}", e);
                        process::exit(-1);
                    }
                };
                let (s, handle) = manager.start().await;

                let signals = Signals::new(&[
                    signal_hook::SIGTERM,
                    signal_hook::SIGINT,
                    signal_hook::SIGQUIT,
                ])
                .unwrap();
                let sig_handle = signals.handle();

                let mut signals = signals.fuse();
                if let Some(signal) = signals.next().await {
                    match signal {
                        signal_hook::SIGTERM | signal_hook::SIGINT | signal_hook::SIGQUIT => {
                            log::trace!("Received stop signal closing...");
                            manager.stop(s).await.unwrap();
                        }
                        _ => unreachable!(),
                    }
                }

                // let ctrlc = CtrlC::new().expect("Unable to create Ctrl-C handler");
                // let mut stream = ctrlc.enumerate().take(1);
                // stream.next().await;
                // log::trace!("Received Ctrl-C start teardown");

                handle.await.unwrap();

                // Terminate the signal stream.
                sig_handle.close();

                log::trace!("Unmounting /sys ...");
                if let Err(e) =
                    nix::mount::umount2(Path::new("/sys"), nix::mount::MntFlags::MNT_DETACH)
                {
                    log::error!("umount2 error {}", e);
                    process::exit(-1);
                }

                log::trace!("Unmounting {} ...", NETNS_PATH);
                if let Err(e) =
                    nix::mount::umount2(Path::new(NETNS_PATH), nix::mount::MntFlags::MNT_DETACH)
                {
                    log::error!("umount2 error {}", e);
                    process::exit(-1);
                }
            }
            async_std::task::block_on(async { __main(args).await });
            log::info!("Bye!");
        }
    }
}

impl NSManager {
    pub async fn new(
        z: Arc<zenoh::net::Session>,
        pid: u32,
        uuid: Uuid,
        rt: tokio::runtime::Runtime,
    ) -> FResult<Self> {
        // This will disappear once netlink merges async-std support
        let handle = rt
            .spawn_blocking(|| {
                let (connection, handle, _) = new_connection().unwrap();
                tokio::spawn(connection);
                handle
            })
            .await
            .map_err(|e| FError::NetworkingError(format!("{}", e)))?;

        let state = NSManagerState {
            tokio_rt: rt,
            nl_handler: handle,
        };

        Ok(Self {
            z,
            pid,
            uuid,
            state: Arc::new(RwLock::new(state)),
        })
    }

    async fn run(&self, stop: async_std::channel::Receiver<()>) -> FResult<()> {
        log::info!("Network Namespace Manager main loop starting...");
        let ns_manager_server = self
            .clone()
            .get_namespace_manager_server(self.z.clone(), Some(self.uuid));

        let (stopper, _h) = ns_manager_server.connect().await?;
        ns_manager_server.initialize().await?;
        ns_manager_server.register().await?;

        let (sender, handle) = ns_manager_server.start().await?;

        log::trace!("Interfaces in namespace {:?}", self.dump_links().await);

        stop.recv().await;

        ns_manager_server.stop(sender).await?;
        ns_manager_server.unregister().await?;
        ns_manager_server.disconnect(stopper).await?;

        log::info!("Network Namespace Manager main loop exiting");
        Ok(())
    }

    pub async fn start(
        &mut self,
    ) -> (
        async_std::channel::Sender<()>,
        async_std::task::JoinHandle<FResult<()>>,
    ) {
        let (s, r) = async_std::channel::bounded::<()>(1);
        let plugin = self.clone();
        let h = async_std::task::spawn_blocking(move || {
            async_std::task::block_on(async { plugin.run(r).await })
        });
        (s, h)
    }

    pub async fn stop(&self, stop: async_std::channel::Sender<()>) -> FResult<()> {
        log::info!("Stopping...");
        stop.send(()).await;
        log::info!("Stopped");
        Ok(())
    }

    async fn create_bridge(&self, br_name: String) -> FResult<()> {
        log::trace!("create_bridge {}", br_name);
        let mut state = self.state.write().await;
        state
            .tokio_rt
            .block_on(async {
                state
                    .nl_handler
                    .link()
                    .add()
                    .bridge(br_name)
                    .execute()
                    .await
            })
            .map_err(|e| FError::NetworkingError(format!("{}", e)))
    }

    async fn create_veth(&self, iface_i: String, iface_e: String) -> FResult<()> {
        let mut state = self.state.write().await;
        state
            .tokio_rt
            .block_on(async {
                state
                    .nl_handler
                    .link()
                    .add()
                    .veth(iface_i, iface_e)
                    .execute()
                    .await
            })
            .map_err(|e| FError::NetworkingError(format!("{}", e)))
    }

    async fn create_vlan(&self, iface: String, dev: String, tag: u16) -> FResult<()> {
        let mut state = self.state.write().await;
        state
            .tokio_rt
            .block_on(async {
                let mut links = state.nl_handler.link().get().set_name_filter(dev).execute();
                if let Some(link) = links
                    .try_next()
                    .await
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))?
                {
                    state
                        .nl_handler
                        .link()
                        .add()
                        .vlan(iface, link.header.index, tag)
                        .execute()
                        .await
                        .map_err(|e| FError::NetworkingError(format!("{}", e)))
                } else {
                    Err(FError::NotFound)
                }
            })
            .map_err(|e| FError::NetworkingError(format!("{}", e)))
    }

    async fn create_mcast_vxlan(
        &self,
        iface: String,
        dev: String,
        vni: u32,
        mcast_addr: IPAddress,
        port: u16,
    ) -> FResult<()> {
        log::trace!(
            "create_mcast_vxlan {} {} {} {} {}",
            iface,
            dev,
            vni,
            mcast_addr,
            port
        );
        let mut state = self.state.write().await;
        state
            .tokio_rt
            .block_on(async {
                let mut links = state.nl_handler.link().get().set_name_filter(dev).execute();
                if let Some(link) = links
                    .try_next()
                    .await
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))?
                {
                    let vxlan = state
                        .nl_handler
                        .link()
                        .add()
                        .vxlan(iface, vni)
                        .link(link.header.index);

                    let vxlan = match mcast_addr {
                        IPAddress::V4(v4) => vxlan.group(v4),
                        IPAddress::V6(v6) => vxlan.group6(v6),
                    };

                    vxlan
                        .port(port)
                        .execute()
                        .await
                        .map_err(|e| FError::NetworkingError(format!("{}", e)))
                } else {
                    Err(FError::NotFound)
                }
            })
            .map_err(|e| FError::NetworkingError(format!("{}", e)))
    }

    async fn create_ptp_vxlan(
        &self,
        iface: String,
        dev: String,
        vni: u32,
        local_addr: IPAddress,
        remote_addr: IPAddress,
        port: u16,
    ) -> FResult<()> {
        log::trace!(
            "create_ptp_vxlan {} {} {} {} {} {}",
            iface,
            dev,
            vni,
            local_addr,
            remote_addr,
            port
        );
        let mut state = self.state.write().await;
        state
            .tokio_rt
            .block_on(async {
                let mut links = state.nl_handler.link().get().set_name_filter(dev).execute();
                if let Some(link) = links
                    .try_next()
                    .await
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))?
                {
                    let vxlan = state
                        .nl_handler
                        .link()
                        .add()
                        .vxlan(iface, vni)
                        .link(link.header.index);

                    let vxlan = match local_addr {
                        IPAddress::V4(v4) => vxlan.local(v4),
                        IPAddress::V6(v6) => vxlan.local6(v6),
                    };

                    let vxlan = match remote_addr {
                        IPAddress::V4(v4) => vxlan.remote(v4),
                        IPAddress::V6(v6) => vxlan.remote6(v6),
                    };

                    vxlan
                        .port(port)
                        .execute()
                        .await
                        .map_err(|e| FError::NetworkingError(format!("{}", e)))
                } else {
                    Err(FError::NotFound)
                }
            })
            .map_err(|e| FError::NetworkingError(format!("{}", e)))
    }

    async fn del_iface(&self, iface: String) -> FResult<()> {
        log::trace!("del_iface {}", iface);
        let mut state = self.state.write().await;
        state.tokio_rt.block_on(async {
            let mut links = state
                .nl_handler
                .link()
                .get()
                .set_name_filter(iface)
                .execute();
            if let Some(link) = links
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                state
                    .nl_handler
                    .link()
                    .del(link.header.index)
                    .execute()
                    .await
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))
            } else {
                Err(FError::NotFound)
            }
        })
    }

    async fn set_iface_master(&self, iface: String, master: String) -> FResult<()> {
        log::trace!("set_iface_master {} {}", iface, master);
        let mut state = self.state.write().await;
        state.tokio_rt.block_on(async {
            let mut links = state
                .nl_handler
                .link()
                .get()
                .set_name_filter(iface)
                .execute();
            if let Some(link) = links
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                let mut masters = state
                    .nl_handler
                    .link()
                    .get()
                    .set_name_filter(master)
                    .execute();
                if let Some(master) = masters
                    .try_next()
                    .await
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))?
                {
                    state
                        .nl_handler
                        .link()
                        .set(link.header.index)
                        .master(master.header.index)
                        .execute()
                        .await
                        .map_err(|e| FError::NetworkingError(format!("{}", e)))
                } else {
                    log::error!("set_iface_master master not found");
                    Err(FError::NotFound)
                }
            } else {
                log::error!("set_iface_master iface not found");
                Err(FError::NotFound)
            }
        })
    }

    async fn del_iface_master(&self, iface: String) -> FResult<()> {
        log::trace!("del_iface_master {}", iface);
        let mut state = self.state.write().await;
        state.tokio_rt.block_on(async {
            let mut links = state
                .nl_handler
                .link()
                .get()
                .set_name_filter(iface)
                .execute();
            if let Some(link) = links
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                state
                    .nl_handler
                    .link()
                    .set(link.header.index)
                    .nomaster()
                    .execute()
                    .await
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))
            } else {
                log::error!("del_iface_master iface not found");
                Err(FError::NotFound)
            }
        })
    }

    async fn add_iface_address(&self, iface: String, addr: IPAddress, prefix: u8) -> FResult<()> {
        let mut state = self.state.write().await;
        state.tokio_rt.block_on(async {
            let mut links = state
                .nl_handler
                .link()
                .get()
                .set_name_filter(iface)
                .execute();
            if let Some(link) = links
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                state
                    .nl_handler
                    .address()
                    .add(link.header.index, addr, prefix)
                    .execute()
                    .await
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))
            } else {
                Err(FError::NotFound)
            }
        })
    }

    async fn get_iface_addresses(&self, iface: String) -> FResult<Vec<IPAddress>> {
        log::trace!("get_iface_addresses {}", iface);
        let mut state = self.state.write().await;
        use netlink_packet_route::rtnl::address::nlas::Nla;
        use netlink_packet_route::rtnl::address::AddressMessage;
        state.tokio_rt.block_on(async {
            let mut nl_addresses = Vec::new();
            let mut f_addresses: Vec<IPAddress> = Vec::new();
            let mut links = state
                .nl_handler
                .link()
                .get()
                .set_name_filter(iface.clone())
                .execute();
            if let Some(link) = links
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                let mut addresses = state
                    .nl_handler
                    .address()
                    .get()
                    .set_link_index_filter(link.header.index)
                    .execute();
                while let Some(msg) = addresses
                    .try_next()
                    .await
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))?
                {
                    for nla in &msg.nlas {
                        match nla {
                            Nla::Address(nl_addr) => {
                                nl_addresses.push((msg.header.clone(), nl_addr.clone()))
                            }
                            _ => continue,
                        }
                    }
                }
                for (_, x) in nl_addresses {
                    if x.len() == 4 {
                        let octects: [u8; 4] = [x[0], x[1], x[2], x[3]];
                        f_addresses.push(IPAddress::from(octects))
                    }
                    if x.len() == 16 {
                        let octects: [u8; 16] = [
                            x[0], x[1], x[2], x[3], x[4], x[5], x[6], x[7], x[8], x[9], x[10],
                            x[11], x[12], x[13], x[14], x[15],
                        ];
                        f_addresses.push(IPAddress::from(octects))
                    }
                }
                Ok(f_addresses)
            } else {
                Err(FError::NotFound)
            }
        })
    }

    async fn del_iface_address(&self, iface: String, addr: IPAddress) -> FResult<()> {
        let mut state = self.state.write().await;
        use netlink_packet_route::rtnl::address::nlas::Nla;
        use netlink_packet_route::rtnl::address::AddressMessage;
        state.tokio_rt.block_on(async {
            let octets = match addr {
                IPAddress::V4(a) => a.octets().to_vec(),
                IPAddress::V6(a) => a.octets().to_vec(),
            };
            let mut nl_addresses = Vec::new();
            let mut links = state
                .nl_handler
                .link()
                .get()
                .set_name_filter(iface.clone())
                .execute();
            if let Some(link) = links
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                let mut addresses = state
                    .nl_handler
                    .address()
                    .get()
                    .set_link_index_filter(link.header.index)
                    .execute();
                while let Some(msg) = addresses
                    .try_next()
                    .await
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))?
                {
                    for nla in &msg.nlas {
                        match nla {
                            Nla::Address(nl_addr) => {
                                nl_addresses.push((msg.header.clone(), nl_addr.clone()))
                            }
                            _ => continue,
                        }
                    }
                }
                match nl_addresses.into_iter().find(|(_, x)| *x == octets) {
                    Some((hdr, addr)) => {
                        let msg = AddressMessage {
                            header: hdr,
                            nlas: vec![Nla::Address(addr)],
                        };
                        state
                            .nl_handler
                            .address()
                            .del(msg)
                            .execute()
                            .await
                            .map_err(|e| FError::NetworkingError(format!("{}", e)))?;
                        Ok(())
                    }
                    None => Err(FError::NotFound),
                }
            } else {
                Err(FError::NotFound)
            }
        })
    }

    async fn set_iface_name(&self, iface: String, new_name: String) -> FResult<()> {
        let mut state = self.state.write().await;
        state.tokio_rt.block_on(async {
            let mut links = state
                .nl_handler
                .link()
                .get()
                .set_name_filter(iface)
                .execute();
            if let Some(link) = links
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                state
                    .nl_handler
                    .link()
                    .set(link.header.index)
                    .name(new_name)
                    .execute()
                    .await
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))
            } else {
                Err(FError::NotFound)
            }
        })
    }

    async fn set_iface_mac(&self, iface: String, address: Vec<u8>) -> FResult<()> {
        let mut state = self.state.write().await;
        state.tokio_rt.block_on(async {
            let mut links = state
                .nl_handler
                .link()
                .get()
                .set_name_filter(iface)
                .execute();
            if let Some(link) = links
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                state
                    .nl_handler
                    .link()
                    .set(link.header.index)
                    .address(address)
                    .execute()
                    .await
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))
            } else {
                Err(FError::NotFound)
            }
        })
    }

    async fn set_iface_default_ns(&self, iface: String) -> FResult<()> {
        let mut state = self.state.write().await;
        state.tokio_rt.block_on(async {
            let mut links = state
                .nl_handler
                .link()
                .get()
                .set_name_filter(iface)
                .execute();
            if let Some(link) = links
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                state
                    .nl_handler
                    .link()
                    .set(link.header.index)
                    .setns_by_pid(1)
                    .execute()
                    .await
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))
            } else {
                Err(FError::NotFound)
            }
        })
    }

    async fn set_iface_up(&self, iface: String) -> FResult<()> {
        log::trace!("set_iface_up {}", iface);
        let mut state = self.state.write().await;
        state.tokio_rt.block_on(async {
            let mut links = state
                .nl_handler
                .link()
                .get()
                .set_name_filter(iface)
                .execute();
            if let Some(link) = links
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                state
                    .nl_handler
                    .link()
                    .set(link.header.index)
                    .up()
                    .execute()
                    .await
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))
            } else {
                Err(FError::NotFound)
            }
        })
    }

    async fn set_iface_down(&self, iface: String) -> FResult<()> {
        let mut state = self.state.write().await;
        state.tokio_rt.block_on(async {
            let mut links = state
                .nl_handler
                .link()
                .get()
                .set_name_filter(iface)
                .execute();
            if let Some(link) = links
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                state
                    .nl_handler
                    .link()
                    .set(link.header.index)
                    .down()
                    .execute()
                    .await
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))
            } else {
                Err(FError::NotFound)
            }
        })
    }

    async fn iface_exists(&self, iface: String) -> FResult<bool> {
        log::trace!("iface_exists {}", iface);
        let mut state = self.state.write().await;
        state.tokio_rt.block_on(async {
            let mut links = state
                .nl_handler
                .link()
                .get()
                .set_name_filter(iface)
                .execute();
            if let Some(link) = links
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }

    async fn dump_links(&self) -> FResult<Vec<String>> {
        log::trace!("dump_links");
        let mut ifaces = Vec::new();
        let mut state = self.state.write().await;
        state.tokio_rt.block_on(async {
            let mut links = state.nl_handler.link().get().execute();
            while let Some(msg) = links
                .try_next()
                .await
                .map_err(|e| FError::NetworkingError(format!("{}", e)))?
            {
                for nla in msg.nlas.into_iter() {
                    if let LinkNla::IfName(name) = nla {
                        ifaces.push(name);
                        break;
                    }
                }
            }
            Ok(ifaces)
        })
    }
}

#[znserver]
impl NamespaceManager for NSManager {
    async fn set_virtual_interface_up(&self, iface: String) -> FResult<()> {
        self.set_iface_up(iface).await
    }
    async fn set_virtual_interface_down(&self, iface: String) -> FResult<()> {
        self.set_iface_down(iface).await
    }
    async fn check_virtual_interface_exists(&self, iface: String) -> FResult<bool> {
        self.iface_exists(iface).await
    }
    async fn move_virtual_interface_into_default_ns(&self, iface: String) -> FResult<()> {
        self.set_iface_default_ns(iface).await
    }
    async fn set_virtual_interface_mac(&self, iface: String, address: Vec<u8>) -> FResult<()> {
        self.set_iface_mac(iface, address).await
    }
    async fn set_virtual_interface_name(&self, iface: String, name: String) -> FResult<()> {
        self.set_iface_name(iface, name).await
    }
    async fn del_virtual_interface_address(&self, iface: String, addr: IPAddress) -> FResult<()> {
        self.del_iface_address(iface, addr).await
    }

    async fn get_virtual_interface_addresses(&self, iface: String) -> FResult<Vec<IPAddress>> {
        self.get_iface_addresses(iface).await
    }

    async fn add_virtual_interface_address(
        &self,
        iface: String,
        addr: Option<IpNetwork>,
    ) -> FResult<Vec<IPAddress>> {
        log::trace!("add_virtual_interface_address {} {:?}", iface, addr);
        match addr {
            Some(addr) => {
                self.add_iface_address(iface.clone(), addr.ip(), addr.prefix())
                    .await?;
                self.get_iface_addresses(iface).await
            }
            None => {
                log::trace!("Using DHCP");
                // If the address is None we spawn a DHCP client
                // and then we the the address from netlink
                let mut child = Command::new("dhclient")
                    .arg("-i")
                    .arg(iface.clone())
                    .spawn()
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))?;
                log::trace!("DHCP Client running {}", child.id());
                let res = child
                    .wait()
                    .map_err(|e| FError::NetworkingError(format!("{}", e)))?;
                log::trace!("DHCP Client exited with {:?}", res);
                self.get_iface_addresses(iface).await
            }
        }
    }
    async fn set_virtual_interface_master(&self, iface: String, master: String) -> FResult<()> {
        self.set_iface_master(iface, master).await
    }
    async fn set_virtual_interface_nomaster(&self, iface: String) -> FResult<()> {
        self.del_iface_master(iface).await
    }
    async fn del_virtual_interface(&self, iface: String) -> FResult<()> {
        self.del_iface(iface).await
    }
    async fn add_virtual_interface_ptp_vxlan(
        &self,
        iface: String,
        dev: String,
        vni: u32,
        local_addr: IPAddress,
        remote_addr: IPAddress,
        port: u16,
    ) -> FResult<()> {
        self.create_ptp_vxlan(iface, dev, vni, local_addr, remote_addr, port)
            .await
    }
    async fn add_virtual_interface_mcast_vxlan(
        &self,
        iface: String,
        dev: String,
        vni: u32,
        mcast_addr: IPAddress,
        port: u16,
    ) -> FResult<()> {
        self.create_mcast_vxlan(iface.clone(), dev, vni, mcast_addr, port)
            .await?;
        self.set_iface_up(iface).await
    }
    async fn add_virtual_interface_vlan(
        &self,
        iface: String,
        dev: String,
        tag: u16,
    ) -> FResult<()> {
        self.create_vlan(iface.clone(), dev, tag).await?;
        self.set_iface_up(iface).await
    }
    async fn add_virtual_interface_veth(&self, iface_i: String, iface_e: String) -> FResult<()> {
        self.create_veth(iface_i.clone(), iface_e.clone()).await?;
        self.set_iface_up(iface_i).await?;
        self.set_iface_up(iface_e).await
    }
    async fn add_virtual_interface_bridge(&self, br_name: String) -> FResult<()> {
        self.create_bridge(br_name.clone()).await?;
        self.set_iface_up(br_name).await
    }

    async fn list_interfaces(&self) -> FResult<Vec<String>> {
        self.dump_links().await
    }
}
