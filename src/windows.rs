#![cfg(target_os = "windows")]

use crate::{run_command, TproxyArgs, TproxyState};
use std::net::{IpAddr, Ipv4Addr};

pub fn tproxy_setup(tproxy_args: &TproxyArgs) -> std::io::Result<TproxyState> {
    // 2. Route all traffic to the adapter, here the destination is adapter's gateway
    // command: `route add 0.0.0.0 mask 0.0.0.0 10.1.0.1 metric 6`
    let unspecified = Ipv4Addr::UNSPECIFIED.to_string();
    let gateway = tproxy_args.tun_gateway.to_string();
    let args = &["add", &unspecified, "mask", &unspecified, &gateway, "metric", "6"];
    run_command("route", args)?;
    #[cfg(feature = "log")]
    log::info!("route {:?}", args);

    let (original_gateway, _) = get_default_gateway()?;

    for bypass_ip in tproxy_args.bypass_ips.iter() {
        do_bypass_ip(*bypass_ip, original_gateway)?;
    }
    if tproxy_args.bypass_ips.is_empty() && !crate::is_private_ip(tproxy_args.proxy_addr.ip()) {
        do_bypass_ip(tproxy_args.proxy_addr.ip(), original_gateway)?;
    }

    // 1. Setup the adapter's DNS
    // command: `netsh interface ip set dns "utun3" static 10.0.0.1`
    let tun_name = format!("\"{}\"", tproxy_args.tun_name);
    let args = &["interface", "ip", "set", "dns", &tun_name, "static", &gateway];
    run_command("netsh", args)?;
    #[cfg(feature = "log")]
    log::info!("netsh {:?}", args);

    let restore = TproxyState {
        tproxy_args: Some(tproxy_args.clone()),
        gateway: Some(original_gateway),
        ..TproxyState::default()
    };
    crate::store_intermediate_state(&restore)?;

    Ok(restore)
}

fn do_bypass_ip(bypass_ip: IpAddr, original_gateway: IpAddr) -> std::io::Result<()> {
    // route the bypass ip to the original gateway
    // command: `route add bypass_ip original_gateway metric 1`
    let args = &["add", &bypass_ip.to_string(), &original_gateway.to_string(), "metric", "1"];
    run_command("route", args)?;
    #[cfg(feature = "log")]
    log::info!("route {:?}", args);
    Ok(())
}

pub fn tproxy_remove(tproxy_restore: Option<TproxyState>) -> std::io::Result<()> {
    let mut state = match tproxy_restore {
        Some(restore) => restore,
        None => crate::retrieve_intermediate_state()?,
    };

    let err = std::io::Error::new(std::io::ErrorKind::InvalidData, "tproxy_args is None");
    let tproxy_args = state.tproxy_args.as_ref().ok_or(err)?;

    let err = std::io::Error::new(std::io::ErrorKind::Other, "No default gateway found");
    let original_gateway = state.gateway.take().ok_or(err)?;
    let unspecified = Ipv4Addr::UNSPECIFIED.to_string();

    // 0. delete persistent route
    // command: `route -p delete 0.0.0.0 mask 0.0.0.0 10.0.0.1`
    let gateway = tproxy_args.tun_gateway.to_string();
    let args = &["-p", "delete", &unspecified, "mask", &unspecified, &gateway];
    if let Err(_err) = run_command("route", args) {
        #[cfg(feature = "log")]
        log::debug!("command \"route {:?}\" error: {}", args, _err);
    }

    // Remove bypass ips
    // command: `route delete bypass_ip`
    for bypass_ip in tproxy_args.bypass_ips.iter() {
        let args = &["delete", &bypass_ip.to_string()];
        if let Err(_err) = run_command("route", args) {
            #[cfg(feature = "log")]
            log::debug!("command \"route {:?}\" error: {}", args, _err);
        }
    }
    if tproxy_args.bypass_ips.is_empty() && !crate::is_private_ip(tproxy_args.proxy_addr.ip()) {
        let bypass_ip = tproxy_args.proxy_addr.ip();
        let args = &["delete", &bypass_ip.to_string()];
        if let Err(_err) = run_command("route", args) {
            #[cfg(feature = "log")]
            log::debug!("command \"route {:?}\" error: {}", args, _err);
        }
    }

    // 1. Remove current adapter's route
    // command: `route delete 0.0.0.0 mask 0.0.0.0`
    let args = &["delete", &unspecified, "mask", &unspecified];
    if let Err(_err) = run_command("route", args) {
        #[cfg(feature = "log")]
        log::debug!("command \"route {:?}\" error: {}", args, _err);
    }

    // 2. Add back the original gateway route
    // command: `route add 0.0.0.0 mask 0.0.0.0 original_gateway metric 200`
    let original_gateway = original_gateway.to_string();
    let args = &["add", &unspecified, "mask", &unspecified, &original_gateway, "metric", "200"];
    if let Err(_err) = run_command("route", args) {
        #[cfg(feature = "log")]
        log::debug!("command \"route {:?}\" error: {}", args, _err);
    }

    // remove the record file anyway
    let _ = std::fs::remove_file(crate::get_state_file_path());

    Ok(())
}

pub(crate) fn get_default_gateway() -> std::io::Result<(IpAddr, String)> {
    let cmd = "Get-WmiObject -Class Win32_NetworkAdapterConfiguration -Filter IPEnabled=TRUE | ForEach-Object { $_.DefaultIPGateway }";
    let gateways = run_command("powershell", &["-Command", cmd])?;

    let stdout = String::from_utf8_lossy(&gateways).into_owned();
    let lines: Vec<&str> = stdout.lines().collect();

    let mut ipv4_gateway = None;
    let mut ipv6_gateway = None;

    for line in lines {
        if let Ok(ip) = <IpAddr as std::str::FromStr>::from_str(line) {
            match ip {
                IpAddr::V4(_) => {
                    ipv4_gateway = Some(ip);
                    break;
                }
                IpAddr::V6(_) => {
                    ipv6_gateway = Some(ip);
                }
            }
        }
    }

    let err = std::io::Error::new(std::io::ErrorKind::Other, "No default gateway found");
    let addr = ipv4_gateway.or(ipv6_gateway).ok_or(err)?;
    let iface = get_default_gateway_interface()?;
    Ok((addr, iface))
}

pub(crate) fn get_default_gateway_interface() -> std::io::Result<String> {
    let cmd = "Get-WmiObject -Class Win32_NetworkAdapter | Where-Object { $_.NetConnectionStatus -eq 2 } | Select-Object -First 1 -ExpandProperty NetConnectionID";
    let iface = run_command("powershell", &["-Command", cmd])?;

    let stdout = String::from_utf8_lossy(&iface).into_owned();
    let iface = stdout.trim().to_string();

    Ok(iface)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_default_gateway() {
        let (addr, iface) = get_default_gateway().unwrap();
        println!("addr: {:?}, iface: {}", addr, iface);
    }
}
