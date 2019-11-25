//! A module for instantiating a desired configuration for a distributed
//! computation. This functionality is meant to be executed by all nodes
//! participating in the distributed computation and will take care of
//! configuring the "local" compute node accordingly, by creating a
//! `DDlogServer` instance, a `TcpReceiver`, a `TxnMux`, and file sinks
//! and sources if desired.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::fs::File;
use std::path::Path;
use std::time::Duration;

use differential_datalog::program::RelId;
use differential_datalog::program::Update;
use differential_datalog::program::Val;
use differential_datalog::record::Record;
use differential_datalog::DDlog;

use crate::observe::Observable;
use crate::schema::Addr;
use crate::schema::Node;
use crate::schema::NodeCfg;
use crate::schema::RelCfg;
use crate::schema::Sink;
use crate::schema::Source;
use crate::schema::SysCfg;
use crate::sinks::File as FileSink;
use crate::sources::File as FileSource;
use crate::tcp_channel::TcpReceiver;
use crate::tcp_channel::TcpSender;
use crate::txnmux::TxnMux;
use crate::DDlogServer;

/// A mapping from member address to relation IDs used for describing
/// output relationships.
pub type Outputs = BTreeMap<Addr, HashSet<RelId>>;
/// A mapping from abstract nodes to actual members in the system.
pub type Assignment = BTreeMap<Node, Addr>;

/// Deduce the output streaming relations we require.
///
/// In a nutshell, this function deduces a mapping from all relations on
/// a node to other nodes that have relations that have this relation as
/// input. Unfortunately doing so is rather costly, as we ultimately
/// have to visit pretty much all relations in the assignment and check
/// them.
fn deduce_outputs(
    addr: &Addr,
    node_cfg: &NodeCfg,
    sys_cfg: &SysCfg,
    assignment: &Assignment,
) -> Outputs {
    node_cfg.keys().fold(Outputs::new(), |mut outputs, rel| {
        sys_cfg
            .iter()
            .filter_map(|(uuid, node_cfg)| {
                assignment.get(uuid).and_then(|other_addr| {
                    if other_addr != addr {
                        Some((other_addr, node_cfg))
                    } else {
                        None
                    }
                })
            })
            .for_each(|(addr, node_cfg)| {
                node_cfg.values().for_each(|rel_cfgs| {
                    rel_cfgs.iter().for_each(|rel_cfg| match rel_cfg {
                        RelCfg::Input(input) => {
                            if input == rel {
                                let rels = outputs.entry(addr.clone()).or_default();
                                let _ = rels.insert(*input);
                            }
                        }
                        RelCfg::Source(_) | RelCfg::Sink(_) => (),
                    })
                })
            });

        outputs
    })
}

/// Deduce the required redirections for a given input/output
/// configuration.
fn deduce_redirects(config: &NodeCfg) -> HashMap<RelId, RelId> {
    config.iter().fold(HashMap::new(), |redirects, (rel, cfg)| {
        cfg.iter().fold(redirects, |mut redirects, rel_cfg| {
            match rel_cfg {
                RelCfg::Input(input) => {
                    // Each of the inputs needs to be redirected to the
                    // relation it feeds into.
                    let _ = redirects.insert(*input, *rel);
                }
                RelCfg::Source(_) | RelCfg::Sink(_) => (),
            }
            redirects
        })
    })
}

/// Create a `DDlogServer` as per the given node configuration.
fn create_server<P>(node_cfg: &NodeCfg) -> Result<DDlogServer<P>, String>
where
    P: Send + DDlog,
{
    let redirects = deduce_redirects(node_cfg);
    // TODO: Should the number of workers be made configurable?
    let program = P::run(2, false, |_, _: &Record, _| {})?;

    Ok(DDlogServer::new(program, redirects))
}

/// Create a transaction multiplexer wrapping the given server.
fn create_txn_mux<P>(server: DDlogServer<P>) -> Result<TxnMux<Update<P::Value>, String>, String>
where
    P: Send + DDlog + 'static,
{
    let mut txnmux = TxnMux::new();
    txnmux
        .subscribe(Box::new(server))
        .map_err(|_| "failed to subscribe DDlogServer to TxnMux")?;

    Ok(txnmux)
}

/// Add as many `TcpSender` objects as required given the provided node
/// configuration.
fn add_tcp_senders<P>(server: &mut DDlogServer<P>, outputs: Outputs) -> Result<(), String>
where
    P: DDlog,
{
    // Create streams for the deduced output relations.
    outputs.into_iter().try_for_each(|(addr, rel_ids)| {
        let timeout = Duration::from_secs(30);
        let interval = Duration::from_millis(500);
        let sender = TcpSender::with_retry(&addr, timeout, interval)
            .map_err(|e| format!("failed to connect to node {}: {}", addr, e))?;

        let mut stream = server.add_stream(rel_ids);
        // TODO: What should we really do if we can't subscribe?
        stream
            .subscribe(Box::new(sender))
            .map_err(|_| "failed to subscribe TCP sender".to_string())?;
        Ok(())
    })
}

/// Add a `TcpReceiver` feeding the given server if one is needed given
/// the provided node configuration.
fn add_tcp_receiver<V>(txnmux: &mut TxnMux<Update<V>, String>, addr: &Addr) -> Result<(), String>
where
    V: Val,
{
    let receiver =
        TcpReceiver::new(addr).map_err(|e| format!("failed to create TcpReceiver: {}", e))?;
    txnmux
        .add_observable(Box::new(receiver))
        .map_err(|_| "failed to register TcpReceiver with TxnMux".to_string())?;
    Ok(())
}

/// Deduce a mapping from file sink to a list of relation IDs for the
/// given node configuration.
fn deduce_sinks_or_sources(node_cfg: &NodeCfg, sinks: bool) -> BTreeMap<&Path, HashSet<RelId>> {
    node_cfg
        .iter()
        .fold(BTreeMap::new(), |map, (relid, rel_cfgs)| {
            rel_cfgs.iter().fold(map, |mut map, rel_cfg| {
                match rel_cfg {
                    RelCfg::Sink(sink) if sinks => match sink {
                        Sink::File(path) => {
                            let _ = map.entry(path).or_default().insert(*relid);
                        }
                    },
                    RelCfg::Source(source) if !sinks => match source {
                        Source::File(path) => {
                            let _ = map.entry(path).or_default().insert(*relid);
                        }
                    },
                    _ => (),
                };
                map
            })
        })
}

/// Add file sinks to the given server object, as per the node
/// configuration.
fn add_file_sinks<P>(server: &mut DDlogServer<P>, node_cfg: &NodeCfg) -> Result<(), String>
where
    P: Send + DDlog + 'static,
    P::Convert: Send,
{
    deduce_sinks_or_sources(node_cfg, true)
        .iter()
        .try_for_each(|(path, rel_ids)| {
            let file = File::create(path)
                .map_err(|e| format!("failed to create file {}: {}", path.display(), e))?;
            let sink = FileSink::<P::Convert>::new(file);

            let mut stream = server.add_stream(rel_ids.clone());
            stream.subscribe(Box::new(sink)).map_err(|_| {
                format!(
                    "failed to subscribe file sink {} to DDlogServer",
                    path.display()
                )
            })?;
            Ok(())
        })
}

fn add_file_sources<P>(
    txnmux: &mut TxnMux<Update<P::Value>, String>,
    node_cfg: &NodeCfg,
) -> Result<(), String>
where
    P: DDlog + 'static,
    P::Convert: Send,
{
    deduce_sinks_or_sources(node_cfg, false)
        .iter()
        .try_for_each(|(path, _rel_ids)| {
            let source = FileSource::<P::Convert, _>::new(path);
            txnmux
                .add_observable(Box::new(source))
                .map_err(|_| format!("failed to add file source {} to TxnMux", path.display()))?;
            Ok(())
        })
}

/// Realize the given configuration locally.
// TODO: Right now this function assumes a pristine state (i.e., nothing
//       had been created previously), however we really would want to
//       transition from a previously created state (which happens to be
//       "empty" initially) to the given one.
fn realize<P>(
    addr: &Addr,
    node_cfg: &NodeCfg,
    outputs: Outputs,
) -> Result<Realization<P::Value>, String>
where
    P: Send + DDlog + 'static,
    P::Convert: Send,
{
    let mut server = create_server::<P>(&node_cfg)?;
    add_tcp_senders(&mut server, outputs)?;
    add_file_sinks(&mut server, node_cfg)?;

    let mut txnmux = create_txn_mux(server)?;
    add_tcp_receiver(&mut txnmux, addr)?;
    add_file_sources::<P>(&mut txnmux, node_cfg)?;

    Ok(Realization { txnmux })
}

/// An object representing a realized configuration.
///
/// Right now all clients can do with object of this type is dropping
/// them to tear everything down.
#[derive(Debug)]
pub struct Realization<V>
where
    V: Debug + Send,
{
    /// The transaction multiplexer everything is registered to.
    txnmux: TxnMux<Update<V>, String>,
}

/// Instantiate a configuration on a particular node under the given
/// assignment.
pub fn instantiate<P>(
    sys_cfg: SysCfg,
    addr: &Addr,
    assignment: &Assignment,
) -> Result<Vec<Realization<P::Value>>, String>
where
    P: Send + DDlog + 'static,
    P::Convert: Send,
{
    assignment
        .iter()
        .filter_map(|(uuid, assigned_addr)| {
            if assigned_addr == addr {
                sys_cfg.get(uuid)
            } else {
                None
            }
        })
        .try_fold(Vec::new(), |mut accumulator, node_cfg| {
            // The supplied configuration by design does not
            // include information about output streaming
            // relations, because these can be inferred by
            // looking at the input relations of other nodes.
            // Start by doing exactly that such that we have
            // enough information to fully configure a node
            // locally.
            let outputs = deduce_outputs(&addr, node_cfg, &sys_cfg, assignment);
            realize::<P>(addr, node_cfg, outputs).map(|realization| {
                accumulator.push(realization);
                accumulator
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use maplit::btreemap;
    use maplit::btreeset;
    use maplit::hashset;

    use uuid::Uuid;

    use crate::schema::Source;

    #[test]
    fn file_sink_deduction() {
        let node_cfg = btreemap! {
            0 => btreeset! {
                RelCfg::Source(Source::File(PathBuf::from("input.dat"))),
                RelCfg::Sink(Sink::File(PathBuf::from("output_0_2.dump"))),
            },
            1 => btreeset! {
                RelCfg::Input(2),
                RelCfg::Input(4),
            },
            2 => btreeset! {
                RelCfg::Sink(Sink::File(PathBuf::from("output_0_2.dump"))),
            },
            3 => btreeset! {
                RelCfg::Sink(Sink::File(PathBuf::from("output_3.dump"))),
                RelCfg::Input(0),
            },
        };

        let sinks = deduce_sinks_or_sources(&node_cfg, true);
        assert_eq!(sinks.len(), 2);

        let rel_ids = sinks.get(Path::new("output_0_2.dump")).unwrap();
        assert_eq!(rel_ids.len(), 2);
        assert!(rel_ids.contains(&0));
        assert!(rel_ids.contains(&2));

        let rel_ids = sinks.get(Path::new("output_3.dump")).unwrap();
        assert_eq!(rel_ids.len(), 1);
        assert!(rel_ids.contains(&3));
    }

    #[test]
    fn output_deduction_two_nodes() {
        let uuid0 = Uuid::new_v4();
        let uuid1 = Uuid::new_v4();
        let node0 = Addr::Ip("127.0.0.1:1".parse().unwrap());
        let node1 = Addr::Ip("127.0.0.1:2".parse().unwrap());

        let node0_cfg = btreemap! {
            0 => btreeset! {
                RelCfg::Source(Source::File(PathBuf::from("input.cmd"))),
            },
        };
        let node1_cfg = btreemap! {
            1 => btreeset! {
                RelCfg::Input(0),
            },
        };
        let sys_cfg = btreemap! {
            uuid0 => node0_cfg.clone(),
            uuid1 => node1_cfg.clone(),
        };
        let assignment = btreemap! {
            uuid0 => node0.clone(),
            uuid1 => node1.clone(),
        };

        let outputs = deduce_outputs(&node0, &node0_cfg, &sys_cfg, &assignment);
        let expected = btreemap! {
            node1.clone() => hashset! { 0 },
        };
        assert_eq!(outputs, expected);

        let outputs = deduce_outputs(&node1, &node1_cfg, &sys_cfg, &assignment);
        assert_eq!(outputs, Outputs::new());
    }

    #[test]
    fn output_deduction_three_nodes() {
        let uuid0 = Uuid::new_v4();
        let uuid1 = Uuid::new_v4();
        let uuid2 = Uuid::new_v4();
        let node0 = Addr::Ip("127.0.0.1:1".parse().unwrap());
        let node1 = Addr::Ip("127.0.0.1:2".parse().unwrap());
        let node2 = Addr::Ip("127.0.0.1:3".parse().unwrap());

        let node0_cfg = btreemap! {
            0 => btreeset!{
                RelCfg::Source(Source::File("input0.dat".into())),
            },
            1 => btreeset!{},
        };
        let node1_cfg = btreemap! {
            2 => btreeset!{
                RelCfg::Source(Source::File("input2.dat".into())),
            },
            3 => btreeset!{}
        };
        let node2_cfg = btreemap! {
            4 => btreeset!{
                RelCfg::Input(1)
            },
            5 => btreeset!{
                RelCfg::Input(3)
            },
            6 => btreeset!{
                RelCfg::Sink(Sink::File("node2.dump".into())),
            },
        };

        let sys_cfg = btreemap! {
            uuid0 => node0_cfg.clone(),
            uuid1 => node1_cfg.clone(),
            uuid2 => node2_cfg.clone(),
        };

        let assignment = btreemap! {
            uuid0 => node0.clone(),
            uuid1 => node1.clone(),
            uuid2 => node2.clone(),
        };

        let outputs = deduce_outputs(&node0, &node0_cfg, &sys_cfg, &assignment);
        let expected = btreemap! {
            node2.clone() => hashset! { 1 },
        };
        assert_eq!(outputs, expected);

        let outputs = deduce_outputs(&node1, &node1_cfg, &sys_cfg, &assignment);
        let expected = btreemap! {
            node2.clone() => hashset! { 3 },
        };
        assert_eq!(outputs, expected);

        let outputs = deduce_outputs(&node2, &node2_cfg, &sys_cfg, &assignment);
        let expected = btreemap! {};
        assert_eq!(outputs, expected);
    }
}