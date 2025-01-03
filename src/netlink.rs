#![allow(dead_code)]

use std::{net::Ipv4Addr, os::fd::AsFd};

use anyhow::Result;

use netavark::network::{
    core_utils::open_netlink_sockets,
    netlink::{self, LinkID},
};
use netlink_packet_route::link::{InfoData, InfoKind, InfoVeth, LinkMessage};
use netns_rs::NetNs;

use crate::network;

fn ns_path(ns: &network::Namespace) -> String {
    format!("/var/run/netns/{}", ns.name)
}

pub(crate) fn namespace_apply(namespace: &network::Namespace) -> Result<()> {
    let _ = NetNs::new(&namespace.name)?;
    Ok(())
}

pub(crate) fn namespace_revert(namespace: &network::Namespace) -> Result<()> {
    let ns = NetNs::get(&namespace.name)?;
    let _ = ns.remove()?;
    Ok(())
}

pub(crate) fn bridge_apply(bridge: &network::Bridge) -> Result<()> {
    let mut socket = netlink::Socket::new()?;

    socket.create_link(netlink::CreateLinkOptions::new(
        bridge.name.clone(),
        InfoKind::Bridge,
    ))?;
    let id = socket
        .get_link(LinkID::Name(bridge.name.clone()))?
        .header
        .index;
    socket.add_addr(id, &bridge.addr.clone().into())?;
    socket.set_up(LinkID::ID(id))?;
    Ok(())
}

pub(crate) fn veth_apply(veth: &network::NamespaceVeth, bridge: &network::Bridge) -> Result<()> {
    let (mut host, mut ns) = open_netlink_sockets(&ns_path(&veth.namespace))?;

    let bridge_index = host
        .netlink
        .get_link(LinkID::Name(bridge.name.clone()))?
        .header
        .index;
    let mut peer_opts = netlink::CreateLinkOptions::new(veth.guest(), InfoKind::Veth);
    peer_opts.netns = Some(ns.file.as_fd());
    let mut peer = LinkMessage::default();
    netlink::parse_create_link_options(&mut peer, peer_opts);
    let mut host_veth: netlink::CreateLinkOptions = netlink::CreateLinkOptions::new(veth.host(), InfoKind::Veth);
    host_veth.info_data = Some(InfoData::Veth(InfoVeth::Peer(peer)));
    host_veth.primary_index = bridge_index;
    host.netlink.create_link(host_veth)?;

    let guest_index = ns
        .netlink
        .get_link(LinkID::Name(veth.guest()))?
        .header
        .index;
    ns.netlink
        .add_addr(guest_index, &veth.addr.clone().into())?;

    let lo_index = ns
        .netlink
        .get_link(LinkID::Name("lo".to_string()))?
        .header
        .index;

    ns.netlink.set_up(LinkID::ID(lo_index))?;
    ns.netlink.set_up(LinkID::Name(veth.guest()))?;
    host.netlink.set_up(LinkID::Name(veth.host()))?;

    let default_route = netlink::Route::Ipv4 { 
        dest: ipnet::Ipv4Net::new(Ipv4Addr::new(0, 0, 0, 0), 0)?, 
        gw: bridge.addr.ip4(), 
        metric: None,
    };
    ns.netlink.add_route(&default_route)?;
    Ok(())
}

pub(crate) fn veth_revert(veth: &network::NamespaceVeth) -> Result<()> {
    let mut host = netlink::Socket::new()?;
    let index = host.get_link(LinkID::Name(veth.host()));
    if let Ok(link) = index {
        host.del_link(LinkID::ID(link.header.index))?;
    }
    Ok(())
}
