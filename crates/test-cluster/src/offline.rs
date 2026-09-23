//! The offline gate at the node: egress rules inside every kind node
//! container, and the canaries that show them working.
//!
//! `NetworkPolicy` only constrains Pods, and only in the namespaces it is
//! applied to. The node's containerd, kubelet and every other Pod reach the
//! network through the node container's own stack, so the runner installs an
//! `iptables` chain there, first in `OUTPUT` and `FORWARD`, that returns only
//! for loopback, local addresses, established flows and the cluster's Pod and
//! Service ranges and rejects everything else. Docker's embedded DNS resolver
//! is refused too, so external names do not resolve. kind exposes each node as
//! a Docker container, which is how the rules are installed and the node's
//! canaries run.

use crate::{
    Result,
    cluster::{POD_SUBNET, SERVICE_SUBNET},
    infrastructure,
    process::{Invocation, tail},
    report::{NetworkProbe, Reach},
};
use std::{fmt::Write as _, time::Duration};

const CHAIN: &str = "APSIDES-OFFLINE";
/// Docker's embedded DNS resolver inside every container.
const DOCKER_DNS: &str = "127.0.0.11/32";
/// A public address a probe must not reach.
pub const EXTERNAL_ADDRESS: &str = "1.1.1.1";
/// A public name a probe must not resolve.
pub const EXTERNAL_NAME: &str = "registry-1.docker.io";
/// An image the node does not hold, which a pull could only fetch from a
/// registry.
const ABSENT_IMAGE: &str = "docker.io/library/busybox:1.36.1";

/// The rules, as one shell script run in the node container.
fn rules_script() -> String {
    format!(
        "set -eu
iptables -w -N {CHAIN}
iptables -w -A {CHAIN} -d {DOCKER_DNS} -j REJECT
iptables -w -A {CHAIN} -m addrtype --dst-type LOCAL -j RETURN
iptables -w -A {CHAIN} -o lo -j RETURN
iptables -w -A {CHAIN} -m conntrack --ctstate ESTABLISHED,RELATED -j RETURN
iptables -w -A {CHAIN} -d {POD_SUBNET} -j RETURN
iptables -w -A {CHAIN} -d {SERVICE_SUBNET} -j RETURN
iptables -w -A {CHAIN} -j REJECT
iptables -w -I OUTPUT 1 -j {CHAIN}
iptables -w -I FORWARD 1 -j {CHAIN}
iptables -w -I INPUT 1 -d {DOCKER_DNS} -j REJECT
ip6tables -w -N {CHAIN}
ip6tables -w -A {CHAIN} -m addrtype --dst-type LOCAL -j RETURN
ip6tables -w -A {CHAIN} -o lo -j RETURN
ip6tables -w -A {CHAIN} -m conntrack --ctstate ESTABLISHED,RELATED -j RETURN
ip6tables -w -A {CHAIN} -j REJECT
ip6tables -w -I OUTPUT 1 -j {CHAIN}
ip6tables -w -I FORWARD 1 -j {CHAIN}
"
    )
}

/// Install the rules in `node` and confirm they sit first.
pub async fn block(node: &str) -> Result<()> {
    Invocation::new("docker")
        .args(["exec", node, "sh", "-c", &rules_script()])
        .timeout(Duration::from_secs(60))
        .run()
        .await?;
    installed(node).await
}

/// Fail unless the rules still sit first in every chain they guard.
pub async fn installed(node: &str) -> Result<()> {
    for (tool, chain, rule) in [
        ("iptables", "OUTPUT", format!("-A OUTPUT -j {CHAIN}")),
        ("iptables", "FORWARD", format!("-A FORWARD -j {CHAIN}")),
        (
            "iptables",
            "INPUT",
            format!("-A INPUT -d {DOCKER_DNS} -j REJECT"),
        ),
        ("ip6tables", "OUTPUT", format!("-A OUTPUT -j {CHAIN}")),
        ("ip6tables", "FORWARD", format!("-A FORWARD -j {CHAIN}")),
    ] {
        let listed = Invocation::new("docker")
            .args(["exec", node, tool, "-w", "-S", chain])
            .run()
            .await?;
        if !first_rule_is(&listed, &rule) {
            return infrastructure(format!(
                "the node egress rule is not first in {tool} {chain} on {node}"
            ));
        }
    }
    Ok(())
}

fn first_rule_is(listing: &str, rule: &str) -> bool {
    listing
        .lines()
        .find(|line| line.starts_with("-A "))
        .is_some_and(|line| line.starts_with(rule))
}

/// Packets the chain's final `REJECT` has matched.
async fn rejected(node: &str) -> Result<u64> {
    let listed = Invocation::new("docker")
        .args([
            "exec", node, "iptables", "-w", "-L", CHAIN, "-v", "-n", "-x",
        ])
        .run()
        .await?;
    reject_counter(&listed).ok_or_else(|| {
        crate::Error::Infrastructure(format!("cannot read the egress rule counter on {node}"))
    })
}

/// The packet count of the last rule in an `iptables -L -v -x` listing, which
/// must be the catch-all `REJECT`.
fn reject_counter(listing: &str) -> Option<u64> {
    let last = listing.lines().rev().find(|line| !line.trim().is_empty())?;
    let fields = last.split_whitespace().collect::<Vec<_>>();
    if fields.get(2) != Some(&"REJECT") {
        return None;
    }
    fields.first()?.parse().ok()
}

/// Run `script` with bash in the node and report whether it exited zero.
async fn succeeds(node: &str, script: &str) -> Result<(bool, String)> {
    let output = Invocation::new("docker")
        .args(["exec", node, "bash", "-c", script])
        .timeout(Duration::from_secs(90))
        .output()
        .await?;
    Ok((output.success, tail(&output.stderr, 400)))
}

fn probe(
    id: &str,
    node: &str,
    target: String,
    expected: Reach,
    observed: Reach,
    detail: String,
) -> NetworkProbe {
    NetworkProbe {
        id: id.into(),
        origin: format!("node {node}"),
        target,
        expected,
        observed,
        detail,
    }
}

/// A TCP connection from the node to the external address. It must fail, and
/// the rule's `REJECT` counter must move, so the failure is the rule's.
pub async fn external_tcp(id: &str, node: &str) -> Result<NetworkProbe> {
    let before = rejected(node).await?;
    let (connected, stderr) = succeeds(
        node,
        &format!("timeout 5 bash -c 'exec 3<>/dev/tcp/{EXTERNAL_ADDRESS}/443'"),
    )
    .await?;
    let matched = rejected(node).await?.saturating_sub(before);
    let observed = if connected {
        Reach::Reachable
    } else if matched > 0 {
        Reach::Blocked
    } else {
        Reach::BlockedElsewhere
    };
    Ok(probe(
        id,
        node,
        format!("{EXTERNAL_ADDRESS}:443"),
        Reach::Blocked,
        observed,
        format!("reject rule matched {matched} packet(s); {stderr}"),
    ))
}

/// Every node canary and positive control, run right after the rules go in.
pub async fn canaries(node: &str, api_address: &str) -> Result<Vec<NetworkProbe>> {
    let mut probes = vec![external_tcp("node-external-tcp", node).await?];

    let (resolved, stderr) =
        succeeds(node, &format!("timeout 10 getent hosts {EXTERNAL_NAME}")).await?;
    probes.push(probe(
        "node-external-dns",
        node,
        EXTERNAL_NAME.into(),
        Reach::Blocked,
        if resolved {
            Reach::Reachable
        } else {
            Reach::Blocked
        },
        stderr,
    ));

    let (present, _) =
        succeeds(node, &format!("crictl inspecti {ABSENT_IMAGE} >/dev/null")).await?;
    if present {
        return infrastructure(format!(
            "the registry canary image {ABSENT_IMAGE} is already on {node}"
        ));
    }
    let (pulled, stderr) =
        succeeds(node, &format!("timeout 60 crictl pull {ABSENT_IMAGE}")).await?;
    probes.push(probe(
        "node-registry-pull",
        node,
        ABSENT_IMAGE.into(),
        Reach::Blocked,
        if pulled {
            Reach::Reachable
        } else {
            Reach::Blocked
        },
        stderr,
    ));

    // The same connection method must reach the API server's Service
    // address, or a refused external connection would prove nothing.
    let (connected, stderr) = succeeds(
        node,
        &format!("timeout 5 bash -c 'exec 3<>/dev/tcp/{api_address}/443'"),
    )
    .await?;
    probes.push(probe(
        "node-api-server-service",
        node,
        format!("{api_address}:443"),
        Reach::Reachable,
        if connected {
            Reach::Reachable
        } else {
            Reach::Blocked
        },
        stderr,
    ));
    Ok(probes)
}

/// Results of a probe Pod, from the lines its script printed.
pub fn pod_results(logs: &str, origin: &str, probes: &[PodProbe]) -> Vec<NetworkProbe> {
    probes
        .iter()
        .map(|item| {
            let line = logs
                .lines()
                .filter_map(|line| line.strip_prefix("APSIDES_PROBE "))
                .filter_map(|line| line.split_once(' '))
                .find(|(id, _)| *id == item.id)
                .map(|(_, result)| result.trim());
            let (result, detail) = line
                .and_then(|line| line.split_once(' '))
                .unwrap_or_else(|| (line.unwrap_or_default(), ""));
            let observed = match result {
                "reachable" => Reach::Reachable,
                "blocked" => Reach::Blocked,
                _ => Reach::NotRun,
            };
            NetworkProbe {
                id: item.id.into(),
                origin: origin.into(),
                target: item.target.clone(),
                expected: item.expected,
                observed,
                detail: detail.into(),
            }
        })
        .collect()
}

/// One connection attempt a probe Pod makes with `nc` or `nslookup`.
#[derive(Clone, Debug)]
pub struct PodProbe {
    pub id: &'static str,
    pub target: String,
    pub command: String,
    pub expected: Reach,
    /// Retry while the connection still succeeds, for a denial that a
    /// `NetworkPolicy` applies only once the CNI has seen the new Pod.
    pub settle: bool,
}

impl PodProbe {
    /// A TCP connection to `host:port`.
    pub fn tcp(id: &'static str, host: &str, port: u16, expected: Reach) -> Self {
        Self {
            id,
            target: format!("{host}:{port}"),
            command: format!("nc -z -w 5 {host} {port}"),
            expected,
            settle: false,
        }
    }

    /// Retry, up to a minute, until the connection is refused. The number of
    /// attempts it took is reported with the result.
    #[must_use]
    pub const fn settling(mut self) -> Self {
        self.settle = true;
        self
    }

    /// A DNS lookup of `name`.
    pub fn dns(id: &'static str, name: &str, expected: Reach) -> Self {
        Self {
            id,
            target: name.into(),
            command: format!("timeout 10 nslookup {name}"),
            expected,
            settle: false,
        }
    }
}

/// The script a probe Pod runs: one result line per probe, always exiting
/// zero so every probe runs.
pub fn pod_script(probes: &[PodProbe]) -> String {
    let mut script = String::new();
    for item in probes {
        if item.settle {
            let _ = writeln!(
                script,
                "n=1; while {} >/dev/null 2>&1 && [ $n -lt 30 ]; do n=$((n+1)); sleep 2; done",
                item.command
            );
            let _ = writeln!(
                script,
                "if {} >/dev/null 2>&1; then echo 'APSIDES_PROBE {id} reachable'; else echo \"APSIDES_PROBE {id} blocked after $n attempt(s)\"; fi",
                item.command,
                id = item.id,
            );
            continue;
        }
        let _ = writeln!(
            script,
            "if {} >/dev/null 2>&1; then echo 'APSIDES_PROBE {id} reachable'; else echo 'APSIDES_PROBE {id} blocked'; fi",
            item.command,
            id = item.id,
        );
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules_allow_only_local_cluster_and_established_traffic() {
        let script = rules_script();
        assert!(script.contains(&format!("-d {POD_SUBNET} -j RETURN")));
        assert!(script.contains(&format!("-d {SERVICE_SUBNET} -j RETURN")));
        assert!(script.contains("-I OUTPUT 1 -j APSIDES-OFFLINE"));
        assert!(script.contains("ip6tables -w -I FORWARD 1 -j APSIDES-OFFLINE"));
        // Docker's resolver is refused before loopback is allowed.
        let dns = script.find("-d 127.0.0.11/32 -j REJECT");
        let loopback = script.find("-o lo -j RETURN");
        assert!(dns < loopback && dns.is_some());
        assert_eq!(
            script
                .lines()
                .filter(|line| line.ends_with(&format!("-A {CHAIN} -j REJECT")))
                .count(),
            2
        );
    }

    #[test]
    fn rule_order_and_counters_are_read_from_iptables_listings() {
        let listing =
            "-P OUTPUT ACCEPT\n-A OUTPUT -j APSIDES-OFFLINE\n-A OUTPUT -j KUBE-FIREWALL\n";
        assert!(first_rule_is(listing, "-A OUTPUT -j APSIDES-OFFLINE"));
        assert!(!first_rule_is(
            "-P OUTPUT ACCEPT\n-A OUTPUT -j KUBE-FIREWALL\n-A OUTPUT -j APSIDES-OFFLINE\n",
            "-A OUTPUT -j APSIDES-OFFLINE"
        ));
        let counters = "Chain APSIDES-OFFLINE (2 references)\n    pkts      bytes target     prot opt in     out     source               destination\n       0        0 RETURN     0    --  *      *       0.0.0.0/0            10.96.0.0/16\n      10      600 REJECT     0    --  *      *       0.0.0.0/0            0.0.0.0/0            reject-with icmp-port-unreachable\n";
        assert_eq!(reject_counter(counters), Some(10));
        assert_eq!(
            reject_counter("       0        0 RETURN     0    --  *  *  0.0.0.0/0  10.96.0.0/16\n"),
            None
        );
    }

    #[test]
    fn pod_probes_report_each_line_and_flag_missing_ones() {
        let probes = [
            PodProbe::tcp("reach", "sentinel", 8080, Reach::Reachable),
            PodProbe::dns("dns", EXTERNAL_NAME, Reach::Blocked),
            PodProbe::tcp("silent", EXTERNAL_ADDRESS, 443, Reach::Blocked),
        ];
        let script = pod_script(&probes);
        assert!(script.contains("nc -z -w 5 sentinel 8080"));
        assert!(script.contains("timeout 10 nslookup registry-1.docker.io"));
        let settled = PodProbe::tcp("policy", "sentinel", 8080, Reach::Blocked).settling();
        assert!(
            pod_script(std::slice::from_ref(&settled)).contains("while nc -z -w 5 sentinel 8080")
        );
        let results = pod_results(
            "APSIDES_PROBE policy blocked after 3 attempt(s)\n",
            "workload",
            &[settled],
        );
        assert!(results[0].passed());
        assert_eq!(results[0].detail, "after 3 attempt(s)");
        let results = pod_results(
            "APSIDES_PROBE reach reachable\nAPSIDES_PROBE dns blocked\n",
            "outside",
            &probes,
        );
        assert!(results[0].passed() && results[1].passed());
        assert_eq!(results[2].observed, Reach::NotRun);
        assert!(!results[2].passed());
    }
}
