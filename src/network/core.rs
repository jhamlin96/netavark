use crate::network::types::NetAddress;
use crate::network::{core_utils, types};
use ipnet;
use log::debug;
use log::warn;
use nix::sched;
use rand::Rng;
use std::collections::HashMap;
use std::fs::File;
use std::io::Error;
use std::net::IpAddr;
use std::os::unix::prelude::*;
use std::thread;

pub struct Core {
    pub networkns: String,
}

impl Core {
    pub fn bridge_per_podman_network(
        per_network_opts: &types::PerNetworkOptions,
        network: &types::Network,
        netns: &str,
    ) -> Result<types::StatusBlock, std::io::Error> {
        //  StatusBlock response
        let mut response = types::StatusBlock {
            interfaces: Some(HashMap::new()),
        };
        // get bridge name
        let bridge_name: String = network.network_interface.as_ref().unwrap().to_owned();
        // static ip vector
        let mut address_vector = Vec::new();
        // gateway ip vector
        let mut gw_ipaddr_vector = Vec::new();
        // network addresses for response
        let mut response_net_addresses: Vec<NetAddress> = Vec::new();
        // interfaces map, but we only ever expect one, for response
        let mut interfaces: HashMap<String, types::NetInterface> = HashMap::new();

        let container_veth_name: String = per_network_opts.interface_name.to_owned();
        let static_ips: &Vec<IpAddr> = per_network_opts.static_ips.as_ref().unwrap();

        //we have the bridge name but we must iterate for all the available gateways
        for (idx, subnet) in network.subnets.iter().flatten().enumerate() {
            let subnet_mask_cidr = subnet.subnet.prefix_len();

            if let Some(gw) = subnet.gateway {
                let gw_net = match gw {
                    IpAddr::V4(gw4) => match ipnet::Ipv4Net::new(gw4, subnet_mask_cidr) {
                        Ok(dest) => ipnet::IpNet::from(dest),
                        Err(err) => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::Other,
                                format!(
                                    "failed to parse address {}/{}: {}",
                                    gw4, subnet_mask_cidr, err
                                ),
                            ))
                        }
                    },
                    IpAddr::V6(gw6) => match ipnet::Ipv6Net::new(gw6, subnet_mask_cidr) {
                        Ok(dest) => ipnet::IpNet::from(dest),
                        Err(err) => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::Other,
                                format!(
                                    "failed to parse address {}/{}: {}",
                                    gw6, subnet_mask_cidr, err
                                ),
                            ))
                        }
                    },
                };

                gw_ipaddr_vector.push(gw_net)
            }

            // Build up response information
            let container_address: ipnet::IpNet =
                match format!("{}/{}", static_ips[idx].to_string(), subnet_mask_cidr).parse() {
                    Ok(i) => i,
                    Err(e) => {
                        return Err(Error::new(std::io::ErrorKind::Other, e));
                    }
                };
            // Add the IP to the address_vector
            address_vector.push(container_address);
            response_net_addresses.push(types::NetAddress {
                gateway: subnet.gateway,
                ipnet: container_address,
            });
        }
        debug!("Container veth name: {:?}", container_veth_name);
        debug!("Brige name: {:?}", bridge_name);
        debug!("IP address for veth vector: {:?}", address_vector);
        debug!("Gateway ip address vector: {:?}", gw_ipaddr_vector);

        let container_veth_mac = match Core::add_bridge_and_veth(
            &bridge_name,
            address_vector,
            gw_ipaddr_vector,
            &container_veth_name,
            netns,
        ) {
            Ok(addr) => addr,
            Err(err) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("failed configure bridge and veth interface: {}", err),
                ))
            }
        };
        debug!("Container veth mac: {:?}", container_veth_mac);
        let interface = types::NetInterface {
            mac_address: container_veth_mac,
            subnets: Option::from(response_net_addresses),
        };
        // Add interface to interfaces (part of StatusBlock)
        interfaces.insert(container_veth_name, interface);
        let _ = response.interfaces.insert(interfaces);
        Ok(response)
    }

    pub fn add_bridge_and_veth(
        br_name: &str,
        netns_ipaddr: Vec<ipnet::IpNet>,
        gw_ipaddr: Vec<ipnet::IpNet>,
        container_veth_name: &str,
        netns: &str,
    ) -> Result<String, std::io::Error> {
        //copy subnet masks and gateway ips since we are going to use it later
        let mut gw_ipaddr_clone = Vec::new();
        for gw_ip in &gw_ipaddr {
            gw_ipaddr_clone.push(*gw_ip)
        }
        //call configure bridge
        let _ = match core_utils::CoreUtils::configure_bridge_async(br_name, gw_ipaddr) {
            Ok(_) => (),
            Err(err) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("failed while configuring network interface {}:", err),
                ))
            }
        };

        let host_veth_name = format!("veth{:x}", rand::thread_rng().gen::<u32>());

        let _ = match core_utils::CoreUtils::configure_veth_async(
            &host_veth_name,
            container_veth_name,
            br_name,
            netns,
        ) {
            Ok(_) => (),
            Err(err) => {
                // it seems something went wrong
                // we must not leave dangling interfaces
                // otherwise cleanup would become mess
                // try removing leaking interfaces from host
                if let Err(er) = core_utils::CoreUtils::remove_interface(&host_veth_name) {
                    warn!("failed while cleaning up interfaces: {}", er);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("failed while configuring network interface {}:", err),
                ));
            }
        };

        //bridge and veth configured successfully
        //do we want mac ?
        //TODO: we can verify MAC here

        match File::open(&netns) {
            Ok(netns_file) => {
                let netns_fd = netns_file.as_raw_fd();
                //clone values before spwaning thread in new namespace
                let container_veth_name_clone: String = container_veth_name.to_owned();
                // So complicated cloning for threads ?
                // TODO: simplify this later
                let mut netns_ipaddr_clone = Vec::new();
                for ip in &netns_ipaddr {
                    netns_ipaddr_clone.push(*ip)
                }
                let handle = thread::spawn(move || -> Result<String, Error> {
                    if let Err(err) = sched::setns(netns_fd, sched::CloneFlags::CLONE_NEWNET) {
                        panic!("failed to setns to fd={}: {}", netns_fd, err);
                    }

                    if let Err(err) = core_utils::CoreUtils::configure_netns_interface_async(
                        &container_veth_name_clone,
                        netns_ipaddr_clone,
                        gw_ipaddr_clone,
                    ) {
                        return Err(err);
                    }
                    debug!(
                        "Configured static up address for {}",
                        container_veth_name_clone
                    );

                    if let Err(er) = core_utils::CoreUtils::turn_up_interface("lo") {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("failed while turning up `lo` in container namespace {}", er),
                        ));
                    }

                    //return MAC address to status block could use this
                    match core_utils::CoreUtils::get_interface_address(&container_veth_name_clone) {
                        Ok(addr) => Ok(addr),
                        Err(err) => Err(err),
                    }
                });
                match handle.join() {
                    Ok(interface_address) => interface_address,
                    Err(err) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("failed to join: {:?}", err),
                        ))
                    }
                }
            }
            Err(err) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("failed to open the netns file: {}", err),
                ))
            }
        }
    }

    pub fn macvlan_per_podman_network(
        per_network_opts: &types::PerNetworkOptions,
        network: &types::Network,
        netns: &str,
    ) -> Result<types::StatusBlock, std::io::Error> {
        //  StatusBlock response
        let mut response = types::StatusBlock {
            dns_search_domains: Some(Vec::new()),
            dns_server_ips: Some(Vec::new()),
            interfaces: Some(HashMap::new()),
        };
        // Does config have a macvlan mode ? I think not
        // Important !! Hardcode MACVLAN_MODE to bridge
        let macvlan_mode: u32 = 4u32;
        // get master interface name
        let master_ifname: String = network.network_interface.as_ref().unwrap().to_owned();
        // static ip vector
        let mut address_vector = Vec::new();
        // network addresses for response
        let mut response_net_addresses: Vec<NetAddress> = Vec::new();
        // interfaces map, but we only ever expect one, for response
        let mut interfaces: HashMap<String, types::NetInterface> = HashMap::new();

        let container_macvlan_name: String = per_network_opts.interface_name.to_owned();
        let static_ips: &Vec<IpAddr> = per_network_opts.static_ips.as_ref().unwrap();

        // prepare a vector of static ips with appropriate cidr
        // we only need static ips so do not process gateway,
        for (idx, subnet) in network.subnets.iter().flatten().enumerate() {
            let subnet_mask_cidr = subnet.subnet.prefix_len();

            // Build up response information
            let container_address: ipnet::IpNet =
                match format!("{}/{}", static_ips[idx].to_string(), subnet_mask_cidr).parse() {
                    Ok(i) => i,
                    Err(e) => {
                        return Err(Error::new(std::io::ErrorKind::Other, e));
                    }
                };
            // Add the IP to the address_vector
            address_vector.push(container_address);
            response_net_addresses.push(types::NetAddress {
                gateway: subnet.gateway, // I dont think we need this in response vector for macvlan ? But let it be for now.
                subnet: container_address,
            });
        }
        debug!("Container macvlan name: {:?}", container_macvlan_name);
        debug!("Master interface name: {:?}", master_ifname);
        debug!("IP address for macvlan: {:?}", address_vector);

        // create macvlan
        let container_macvlan_mac = match Core::add_macvlan(
            &master_ifname,
            &container_macvlan_name,
            macvlan_mode,
            address_vector,
            netns,
        ) {
            Ok(addr) => addr,
            Err(err) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("failed configure macvlan: {}", err),
                ))
            }
        };
        debug!("Container macvlan mac: {:?}", container_macvlan_mac);
        let interface = types::NetInterface {
            mac_address: container_macvlan_mac,
            networks: Option::from(response_net_addresses),
        };
        // Add interface to interfaces (part of StatusBlock)
        interfaces.insert(container_macvlan_name, interface);
        let _ = response.interfaces.insert(interfaces);
        Ok(response)
    }

    pub fn add_macvlan(
        master_ifname: &str,
        container_macvlan: &str,
        macvlan_mode: u32,
        netns_ipaddr: Vec<ipnet::IpNet>,
        netns: &str,
    ) -> Result<String, std::io::Error> {
        let _ = match core_utils::CoreUtils::configure_macvlan_async(
            master_ifname,
            container_macvlan,
            macvlan_mode,
            netns,
        ) {
            Ok(_) => (),
            Err(err) => {
                // it seems something went wrong
                // we must not leave dangling interfaces
                // otherwise cleanup would become mess
                // try removing leaking interfaces from host
                if let Err(er) = core_utils::CoreUtils::remove_interface(container_macvlan) {
                    warn!("failed while cleaning up interfaces: {}", er);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("failed while configuring macvlan {}:", err),
                ));
            }
        };

        match File::open(&netns) {
            Ok(netns_file) => {
                let netns_fd = netns_file.as_raw_fd();
                //clone values before spwaning thread in new namespace
                let container_macvlan_clone: String = container_macvlan.to_owned();
                // So complicated cloning for threads ?
                // TODO: simplify this later
                let _gw_ipaddr_empty = Vec::new(); // we are not using this for macvlan but arg is needed.
                let mut netns_ipaddr_clone = Vec::new();
                for ip in &netns_ipaddr {
                    netns_ipaddr_clone.push(*ip)
                }
                let handle = thread::spawn(move || -> Result<String, Error> {
                    if let Err(err) = sched::setns(netns_fd, sched::CloneFlags::CLONE_NEWNET) {
                        panic!("failed to setns to fd={}: {}", netns_fd, err);
                    }

                    if let Err(err) = core_utils::CoreUtils::configure_netns_interface_async(
                        &container_macvlan_clone,
                        netns_ipaddr_clone,
                        _gw_ipaddr_empty,
                    ) {
                        return Err(err);
                    }
                    debug!(
                        "Configured static up address for {}",
                        container_macvlan_clone
                    );

                    if let Err(er) = core_utils::CoreUtils::turn_up_interface("lo") {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("failed while turning up `lo` in container namespace {}", er),
                        ));
                    }

                    //return MAC address to status block could use this
                    match core_utils::CoreUtils::get_interface_address(&container_macvlan_clone) {
                        Ok(addr) => Ok(addr),
                        Err(err) => Err(err),
                    }
                });
                match handle.join() {
                    Ok(interface_address) => interface_address,
                    Err(err) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("failed to join: {:?}", err),
                        ))
                    }
                }
            }
            Err(err) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("failed to open the netns file: {}", err),
                ))
            }
        }
    }

    pub fn remove_interface_per_podman_network(
        per_network_opts: &types::PerNetworkOptions,
        network: &types::Network,
        netns: &str,
    ) -> Result<(), std::io::Error> {
        let container_veth_name: String = per_network_opts.interface_name.to_owned();
        let _subnets = network.subnets.as_ref().unwrap();

        debug!(
            "Container veth name being removed: {:?}",
            container_veth_name
        );

        if let Err(err) = Core::remove_container_veth(&container_veth_name, netns) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("unable to remove container veth: {}", err),
            ));
        }

        debug!("Container veth removed: {:?}", container_veth_name);

        Ok(())
    }

    fn remove_container_veth(ifname: &str, netns: &str) -> Result<(), std::io::Error> {
        match File::open(netns) {
            Ok(file) => {
                let netns_fd = file.as_raw_fd();
                let container_veth: String = ifname.to_owned();
                let handle = thread::spawn(move || -> Result<(), Error> {
                    if let Err(err) = sched::setns(netns_fd, sched::CloneFlags::CLONE_NEWNET) {
                        panic!(
                            "{}",
                            format!(
                                "failed to setns on container network namespace fd={}: {}",
                                netns_fd, err
                            )
                        )
                    }

                    if let Err(err) = core_utils::CoreUtils::remove_interface(&container_veth) {
                        return Err(err);
                    }

                    Ok(())
                });
                if let Err(err) = handle.join() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("unable to join thread: {:?}", err),
                    ));
                }
            }
            Err(err) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("failed to open network namespace: {}", err),
                ))
            }
        };

        Ok(())
    }
}
