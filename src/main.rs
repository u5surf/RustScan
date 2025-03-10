extern crate shell_words;

mod tui;

mod input;
use input::{Config, Opts, PortRange, ScanOrder};

mod scanner;
use scanner::Scanner;

mod port_strategy;
use port_strategy::PortStrategy;

mod benchmark;
use benchmark::{Benchmark, NamedTimer};

use cidr_utils::cidr::IpCidr;
use colorful::{Color, Colorful};
use futures::executor::block_on;
use rlimit::{getrlimit, setrlimit, Resource};
use std::collections::HashMap;
use std::fs::File;
use std::io::{prelude::*, BufReader};
use std::net::{IpAddr, ToSocketAddrs};
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use trust_dns_resolver::{config::*, Resolver};

extern crate colorful;
extern crate dirs;

// Average value for Ubuntu
const DEFAULT_FILE_DESCRIPTORS_LIMIT: rlimit::rlim = 8000;
// Safest batch size based on experimentation
const AVERAGE_BATCH_SIZE: rlimit::rlim = 3000;

#[macro_use]
extern crate log;

#[cfg(not(tarpaulin_include))]
/// Faster Nmap scanning with Rust
/// If you're looking for the actual scanning, check out the module Scanner
fn main() {
    env_logger::init();
    let mut benchmarks = Benchmark::init();
    let mut rustscan_bench = NamedTimer::start("RustScan");

    let mut opts: Opts = Opts::read();
    let config = Config::read();
    opts.merge(&config);

    debug!("Main() `opts` arguments are {:?}", opts);

    if !opts.greppable && !opts.accessible {
        print_opening(&opts);
    }

    let ips: Vec<IpAddr> = parse_addresses(&opts);

    if ips.is_empty() {
        warning!(
            "No IPs could be resolved, aborting scan.",
            opts.greppable,
            opts.accessible
        );
        std::process::exit(1);
    }

    let ulimit: rlimit::rlim = adjust_ulimit_size(&opts);
    let batch_size: u16 = infer_batch_size(&opts, ulimit);

    let scanner = Scanner::new(
        &ips,
        batch_size,
        Duration::from_millis(opts.timeout.into()),
        opts.tries,
        opts.greppable,
        PortStrategy::pick(opts.range, opts.ports, opts.scan_order),
        opts.accessible,
    );
    debug!("Scanner finished building: {:?}", scanner);

    let mut portscan_bench = NamedTimer::start("Portscan");
    let scan_result = block_on(scanner.run());
    portscan_bench.end();
    benchmarks.push(portscan_bench);

    let mut ports_per_ip = HashMap::new();

    for socket in scan_result {
        ports_per_ip
            .entry(socket.ip())
            .or_insert_with(Vec::new)
            .push(socket.port());
    }

    for ip in ips {
        if ports_per_ip.contains_key(&ip) {
            continue;
        }

        // If we got here it means the IP was not found within the HashMap, this
        // means the scan couldn't find any open ports for it.

        let x = format!("Looks like I didn't find any open ports for {:?}. This is usually caused by a high batch size.
        \n*I used {} batch size, consider lowering it with {} or a comfortable number for your system.
        \n Alternatively, increase the timeout if your ping is high. Rustscan -t 2000 for 2000 milliseconds (2s) timeout.\n",
        ip,
        opts.batch_size,
        "'rustscan -b <batch_size> <ip address>'");
        warning!(x, opts.greppable, opts.accessible);
    }

    let mut nmap_bench = NamedTimer::start("Nmap");
    for (ip, ports) in ports_per_ip.iter_mut() {
        let nmap_str_ports: Vec<String> = ports.into_iter().map(|port| port.to_string()).collect();

        // nmap port style is 80,443. Comma separated with no spaces.
        let ports_str = nmap_str_ports.join(",");

        // if greppable mode is on nmap should not be spawned
        if opts.greppable || opts.no_nmap {
            println!("{} -> [{}]", &ip, ports_str);
            continue;
        }
        detail!("Starting Nmap", opts.greppable, opts.accessible);

        let addr = ip.to_string();
        let user_nmap_args =
            shell_words::split(&opts.command.join(" ")).expect("failed to parse nmap arguments");
        let nmap_args = build_nmap_arguments(&addr, &ports_str, &user_nmap_args, ip.is_ipv6());

        output!(
            format!(
                "The Nmap command to be run is nmap {}\n",
                &nmap_args.join(" ")
            ),
            opts.greppable.clone(),
            opts.accessible.clone()
        );

        // Runs the nmap command and spawns it as a process.
        let mut child = Command::new("nmap")
            .args(&nmap_args)
            .spawn()
            .expect("failed to execute nmap process");

        child.wait().expect("failed to wait on nmap process");
    }

    // To use the runtime benchmark, run the process as: RUST_LOG=info ./rustscan
    nmap_bench.end();
    benchmarks.push(nmap_bench);
    rustscan_bench.end();
    benchmarks.push(rustscan_bench);
    debug!("Benchmarks raw {:?}", benchmarks);
    info!("{}", benchmarks.summary());
}

/// Prints the opening title of RustScan
fn print_opening(opts: &Opts) {
    debug!("Printing opening");
    let s = r#".----. .-. .-. .----..---.  .----. .---.   .--.  .-. .-.
| {}  }| { } |{ {__ {_   _}{ {__  /  ___} / {} \ |  `| |
| .-. \| {_} |.-._} } | |  .-._} }\     }/  /\  \| |\  |
`-' `-'`-----'`----'  `-'  `----'  `---' `-'  `-'`-' `-'
Faster Nmap scanning with Rust."#;
    println!("{}", s.gradient(Color::Green).bold());
    let info = r#"________________________________________
: https://discord.gg/GFrQsGy           :
: https://github.com/RustScan/RustScan :
 --------------------------------------"#;
    println!("{}", info.gradient(Color::Yellow).bold());
    funny_opening!();

    let mut home_dir = match dirs::home_dir() {
        Some(dir) => dir,
        None => panic!("Could not infer config file path."),
    };
    home_dir.push(".rustscan.toml");

    detail!(
        format!("The config file is expected to be at {:?}", home_dir),
        opts.greppable,
        opts.accessible
    );
}

/// Goes through all possible IP inputs (files or via argparsing)
/// Parses the string(s) into IPs
fn parse_addresses(input: &Opts) -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = Vec::new();
    let mut unresolved_addresses: Vec<&str> = Vec::new();
    let resolver =
        &Resolver::new(ResolverConfig::cloudflare_tls(), ResolverOpts::default()).unwrap();

    for address in &input.addresses {
        match parse_address(address, resolver) {
            Ok(parsed_ips) => {
                if !parsed_ips.is_empty() {
                    ips.extend(parsed_ips);
                } else {
                    unresolved_addresses.push(address);
                }
            }
            _ => {
                warning!(
                    format!("Host {:?} could not be resolved.", address),
                    input.greppable,
                    input.accessible
                );
            }
        }
    }

    // If we got to this point this can only be a file path or the wrong input.
    for file_path in unresolved_addresses {
        let file_path = Path::new(file_path);

        if !file_path.is_file() {
            warning!(
                format!("Host {:?} could not be resolved.", file_path),
                input.greppable,
                input.accessible
            );

            continue;
        }

        match read_ips_from_file(file_path, &resolver) {
            Ok(x) => ips.extend(x),
            _ => {
                warning!(
                    format!("Host {:?} could not be resolved.", file_path),
                    input.greppable,
                    input.accessible
                );
            }
        }
    }

    ips
}

/// Given a string, parse it as an host, IP address, or CIDR.
/// This allows us to pass files as hosts or cidr or IPs easily
/// Call this everytime you have a possible IP_or_host
fn parse_address(address: &str, resolver: &Resolver) -> Result<Vec<IpAddr>, std::io::Error> {
    let mut ips: Vec<IpAddr> = Vec::new();

    match IpCidr::from_str(&address) {
        Ok(cidr) => cidr.iter().for_each(|ip| ips.push(ip)),
        _ => match format!("{}:{}", &address, 80).to_socket_addrs() {
            Ok(mut iter) => ips.push(iter.nth(0).unwrap().ip()),
            _ => match resolve_ips_from_host(address, resolver) {
                Ok(hosts) => ips.extend(hosts),
                _ => (),
            },
        },
    };

    Ok(ips)
}

/// Uses DNS to get the IPS assiocated with host
fn resolve_ips_from_host(source: &str, resolver: &Resolver) -> Result<Vec<IpAddr>, std::io::Error> {
    match resolver.lookup_ip(&source) {
        Ok(x) => Ok(x.iter().collect()),
        _ => Ok(Vec::new()),
    }
}

#[cfg(not(tarpaulin_include))]
/// Parses an input file of IPs and uses those
fn read_ips_from_file(
    ips: &std::path::Path,
    resolver: &Resolver,
) -> Result<Vec<std::net::IpAddr>, std::io::Error> {
    let file = File::open(ips)?;
    let reader = BufReader::new(file);

    let mut ips: Vec<std::net::IpAddr> = Vec::new();

    for address_line in reader.lines() {
        match address_line {
            Ok(address) => match parse_address(&address, resolver) {
                Ok(result) => ips.extend(result),
                Err(e) => {
                    debug!("{} is not a valid IP or host", e);
                }
            },
            Err(_) => {
                debug!("Line in file is not valid");
            }
        }
    }
    Ok(ips)
}

#[cfg(not(tarpaulin_include))]
fn build_nmap_arguments<'a>(
    addr: &'a str,
    ports: &'a str,
    user_args: &'a Vec<String>,
    is_ipv6: bool,
) -> Vec<&'a str> {
    let mut arguments: Vec<&str> = user_args.iter().map(AsRef::as_ref).collect();
    arguments.push("-vvv");

    if is_ipv6 {
        arguments.push("-6");
    }

    arguments.push("-p");
    arguments.push(ports);
    arguments.push(addr);

    arguments
}

fn adjust_ulimit_size(opts: &Opts) -> rlimit::rlim {
    if opts.ulimit.is_some() {
        let limit: rlimit::rlim = opts.ulimit.unwrap();

        match setrlimit(Resource::NOFILE, limit, limit) {
            Ok(_) => {
                detail!(
                    format!("Automatically increasing ulimit value to {}.", limit),
                    opts.greppable,
                    opts.accessible
                );
            }
            Err(_) => {
                warning!(
                    "ERROR. Failed to set ulimit value.",
                    opts.greppable,
                    opts.accessible
                );
            }
        }
    }

    let (rlim, _) = getrlimit(Resource::NOFILE).unwrap();

    rlim
}

fn infer_batch_size(opts: &Opts, ulimit: rlimit::rlim) -> u16 {
    let mut batch_size: rlimit::rlim = opts.batch_size.into();

    // Adjust the batch size when the ulimit value is lower than the desired batch size
    if ulimit < batch_size {
        warning!("File limit is lower than default batch size. Consider upping with --ulimit. May cause harm to sensitive servers",
            opts.greppable, opts.accessible
        );

        // When the OS supports high file limits like 8000, but the user
        // selected a batch size higher than this we should reduce it to
        // a lower number.
        if ulimit < AVERAGE_BATCH_SIZE {
            // ulimit is smaller than aveage batch size
            // user must have very small ulimit
            // decrease batch size to half of ulimit
            warning!("Your file limit is very small, which negatively impacts RustScan's speed. Use the Docker image, or up the Ulimit with '--ulimit 5000'. ", opts.greppable, opts.accessible);
            info!("Halving batch_size because ulimit is smaller than average batch size");
            batch_size = ulimit / 2
        } else if ulimit > DEFAULT_FILE_DESCRIPTORS_LIMIT {
            info!("Batch size is now average batch size");
            batch_size = AVERAGE_BATCH_SIZE
        } else {
            batch_size = ulimit - 100
        }
    }
    // When the ulimit is higher than the batch size let the user know that the
    // batch size can be increased unless they specified the ulimit themselves.
    else if ulimit + 2 > batch_size && (opts.ulimit.is_none()) {
        detail!(format!("File limit higher than batch size. Can increase speed by increasing batch size '-b {}'.", ulimit - 100), 
            opts.greppable, opts.accessible);
    }

    batch_size as u16
}

#[cfg(test)]
mod tests {
    use crate::{adjust_ulimit_size, infer_batch_size, parse_addresses, print_opening, Opts};
    use std::net::Ipv4Addr;

    #[test]
    fn batch_size_lowered() {
        let mut opts = Opts::default();
        opts.batch_size = 50_000;
        let batch_size = infer_batch_size(&opts, 120);

        assert!(batch_size < opts.batch_size);
    }

    #[test]
    fn batch_size_lowered_average_size() {
        let mut opts = Opts::default();
        opts.batch_size = 50_000;
        let batch_size = infer_batch_size(&opts, 9_000);

        assert!(batch_size == 3_000);
    }
    #[test]
    fn batch_size_equals_ulimit_lowered() {
        // because ulimit and batch size are same size, batch size is lowered
        // to ULIMIT - 100
        let mut opts = Opts::default();
        opts.batch_size = 50_000;
        let batch_size = infer_batch_size(&opts, 5_000);

        assert!(batch_size == 4_900);
    }
    #[test]
    fn batch_size_adjusted_2000() {
        // ulimit == batch_size
        let mut opts = Opts::default();
        opts.batch_size = 50_000;
        opts.ulimit = Some(2_000);
        let batch_size = adjust_ulimit_size(&opts);

        assert!(batch_size == 2_000);
    }
    #[test]
    fn test_print_opening_no_panic() {
        let mut opts = Opts::default();
        opts.ulimit = Some(2_000);
        // print opening should not panic
        print_opening(&opts);
        assert!(1 == 1);
    }
    #[test]
    fn test_high_ulimit_no_greppable_mode() {
        let mut opts = Opts::default();
        opts.batch_size = 10;
        opts.greppable = false;

        let batch_size = infer_batch_size(&opts, 1_000_000);

        assert!(batch_size == opts.batch_size);
    }

    #[test]
    fn parse_correct_addresses() {
        let mut opts = Opts::default();
        opts.addresses = vec!["127.0.0.1".to_owned(), "192.168.0.0/30".to_owned()];
        let ips = parse_addresses(&opts);

        assert_eq!(
            ips,
            [
                Ipv4Addr::new(127, 0, 0, 1),
                Ipv4Addr::new(192, 168, 0, 0),
                Ipv4Addr::new(192, 168, 0, 1),
                Ipv4Addr::new(192, 168, 0, 2),
                Ipv4Addr::new(192, 168, 0, 3)
            ]
        );
    }

    #[test]
    fn parse_correct_host_addresses() {
        let mut opts = Opts::default();
        opts.addresses = vec!["google.com".to_owned()];
        let ips = parse_addresses(&opts);

        assert_eq!(ips.len(), 1);
    }

    #[test]
    fn parse_correct_and_incorrect_addresses() {
        let mut opts = Opts::default();
        opts.addresses = vec!["127.0.0.1".to_owned(), "im_wrong".to_owned()];
        let ips = parse_addresses(&opts);

        assert_eq!(ips, [Ipv4Addr::new(127, 0, 0, 1),]);
    }

    #[test]
    fn parse_incorrect_addresses() {
        let mut opts = Opts::default();
        opts.addresses = vec!["im_wrong".to_owned(), "300.10.1.1".to_owned()];
        let ips = parse_addresses(&opts);

        assert_eq!(ips.is_empty(), true);
    }
    #[test]
    fn parse_hosts_file_and_incorrect_hosts() {
        // Host file contains IP, Hosts, incorrect IPs, incorrect hosts
        let mut opts = Opts::default();
        opts.addresses = vec!["fixtures/hosts.txt".to_owned()];
        let ips = parse_addresses(&opts);
        assert_eq!(ips.len(), 3);
    }

    #[test]
    fn parse_empty_hosts_file() {
        // Host file contains IP, Hosts, incorrect IPs, incorrect hosts
        let mut opts = Opts::default();
        opts.addresses = vec!["fixtures/empty_hosts.txt".to_owned()];
        let ips = parse_addresses(&opts);
        assert_eq!(ips.len(), 0);
    }

    #[test]
    fn parse_naughty_host_file() {
        // Host file contains IP, Hosts, incorrect IPs, incorrect hosts
        let mut opts = Opts::default();
        opts.addresses = vec!["fixtures/naughty_string.txt".to_owned()];
        let ips = parse_addresses(&opts);
        assert_eq!(ips.len(), 0);
    }
}
