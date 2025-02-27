//! Evaluation order for nodes.
use crate::internal::*;
use bit_set::{self, BitSet};
use std::fmt::{Debug, Display};
use tract_data::itertools::Itertools;

/// Find an evaluation order for a model, using its default inputs and outputs
/// as boundaries.
pub fn eval_order<F, O>(model: &super::Graph<F, O>) -> TractResult<Vec<usize>>
where
    F: Fact + Clone + 'static,
    O: Debug + Display + AsRef<dyn Op> + AsMut<dyn Op> + Clone + 'static,
{
    let inputs = model.input_outlets()?.iter().map(|n| n.node).collect::<Vec<usize>>();
    let targets = model.output_outlets()?.iter().map(|n| n.node).collect::<Vec<usize>>();
    eval_order_for_nodes(model.nodes(), &inputs, &targets, &[])
}

/// Find a working evaluation order for a list of nodes.
pub fn eval_order_for_nodes<F, O>(
    nodes: &[Node<F, O>],
    model_inputs: &[usize],
    model_outputs: &[usize],
    more_dependencies: &[(usize, usize)],
) -> TractResult<Vec<usize>>
where
    F: Fact + Clone + 'static,
    O: Debug + Display + AsRef<dyn Op> + AsMut<dyn Op> + Clone + 'static,
{
    let mut done = bit_set::BitSet::with_capacity(nodes.len());
    let mut order: Vec<usize> = vec![];
    for &model_target in model_outputs {
        if done.contains(model_target) {
            continue;
        }
        let mut current_stack: Vec<(usize, usize)> = vec![(model_target, 0)];
        let mut pending = bit_set::BitSet::with_capacity(nodes.len());
        while let Some((current_node, current_input)) = current_stack.pop() {
            let deps_from_inputs = nodes[current_node].inputs.len();
            let all_deps_count =
                deps_from_inputs + more_dependencies.iter().filter(|a| a.0 == current_node).count();
            if model_inputs.contains(&current_node) || current_input == all_deps_count {
                order.push(current_node);
                done.insert(current_node);
                pending.remove(current_node);
            } else {
                let precursor: usize = nodes[current_node]
                    .inputs
                    .iter()
                    .filter(|n| nodes[n.node].inputs.len() > 0)
                    .map(|n| n.node)
                    .chain(more_dependencies.iter().filter(|a| a.0 == current_node).map(|n| n.1))
                    .chain(
                        nodes[current_node]
                            .inputs
                            .iter()
                            .filter(|n| nodes[n.node].inputs.len() == 0)
                            .map(|n| n.node),
                    )
                    .nth(current_input)
                    .unwrap();
                if done.contains(precursor) {
                    current_stack.push((current_node, current_input + 1));
                } else if pending.contains(precursor) {
                    if log_enabled!(log::Level::Debug) {
                        debug!("Loop detected:");
                        current_stack
                            .iter()
                            .skip_while(|s| s.0 != precursor)
                            .for_each(|n| debug!("  {}", nodes[n.0]));
                    }
                    bail!("Loop detected")
                } else {
                    pending.insert(precursor);
                    current_stack.push((current_node, current_input));
                    current_stack.push((precursor, 0));
                }
            }
        }
    }
    Ok(order)
}

/// Find a working evaluation order for a list of nodes.
pub fn eval_order_for_nodes_memory<F, O>(
    nodes: &[Node<F, O>],
    _model_inputs: &[usize],
    model_outputs: &[usize],
    more_dependencies: &[(usize, usize)],
) -> TractResult<Vec<usize>>
where
    F: Fact + Clone + 'static,
    O: Debug + Display + AsRef<dyn Op> + AsMut<dyn Op> + Clone + 'static,
{
    let mut ups = vec![tvec!(); nodes.len()];
    let mut downs = vec![tvec!(); nodes.len()];
    for (ix, node) in nodes.iter().enumerate() {
        for input in &node.inputs {
            if !ups[ix].contains(&input.node) {
                ups[ix].push(input.node);
                downs[input.node].push(ix);
            }
        }
    }
    for (down, up) in more_dependencies {
        if !ups[*down].contains(up) {
            ups[*down].push(*up);
            downs[*up].push(*down);
        }
    }
    let costs: Vec<usize> = nodes
        .iter()
        .map(|node| {
            node.outputs
                .iter()
                .map(|o| {
                    o.fact
                        .to_typed_fact()
                        .map(|f| {
                            f.datum_type.size_of()
                                * f.shape
                                    .as_concrete()
                                    .map(|dims| dims.iter().product())
                                    .unwrap_or(0)
                        })
                        .unwrap_or(0)
                })
                .sum()
        })
        .collect_vec();
    let mut todo = bit_set::BitSet::with_capacity(nodes.len());
    todo.extend(model_outputs.iter().copied());
    loop {
        let mut up: BitSet = todo.iter().flat_map(|n| ups[n].iter().copied()).collect::<BitSet>();
        up.difference_with(&todo);
        if up.len() == 0 {
            break;
        } else {
            todo.union_with(&up);
        }
    }
    let mut order = vec![];
    let mut active = BitSet::with_capacity(nodes.len());
    let mut candidates = BitSet::with_capacity(nodes.len());
    candidates.extend(todo.iter().filter(|n| ups[*n].len() == 0));
    while todo.len() > 0 {
        let next = candidates
            .iter()
            .filter(|n| !ups[*n].iter().any(|up| todo.contains(*up)))
            .min_by_key(|&candidate| {
                active.clear();
                active.extend(
                    todo.iter()
                        .filter(|it| *it != candidate)
                        .flat_map(|down| ups[down].iter().copied())
                        .filter(|up| *up == candidate || !todo.contains(*up)),
                );
                active.iter().map(|n| costs[n]).sum::<usize>()
            })
            .context("Dependency loop detected.")?;
        order.push(next);
        todo.remove(next);
        candidates.remove(next);
        candidates.extend(
            downs[next]
                .iter()
                .copied()
                .filter(|n| todo.contains(*n) && ups[*n].iter().all(|up| !todo.contains(*up))),
        );
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use crate::internal::*;
    use crate::ops::math;

    #[test]
    fn simple() {
        let mut model = TypedModel::default();
        let a = model.add_source("a", f32::fact([1])).unwrap();
        let b = model.add_const("b", tensor1(&[12.0f32])).unwrap();
        let add = model.wire_node("add", math::add(), &[a, b]).unwrap()[0];
        model.auto_outputs().unwrap();
        assert_eq!(model.eval_order().unwrap(), vec!(a.node, b.node, add.node));
    }

    #[test]
    fn diamond() {
        let mut model = TypedModel::default();
        let a = model.add_source("a", f32::fact([1])).unwrap();
        let add = model.wire_node("add", math::add(), &[a, a]).unwrap()[0];
        model.auto_outputs().unwrap();
        assert_eq!(model.eval_order().unwrap(), vec!(a.node, add.node));
    }

    #[test]
    fn dodge_loop() {
        let mut model = TypedModel::default();
        let a = model.add_source("a", f32::fact([1])).unwrap();
        let add = model.wire_node("add", math::add(), &[a, a]).unwrap()[0];
        let neg = model.wire_node("neg", math::add(), &[add, a]).unwrap()[0];
        model.add_edge(neg, InletId::new(add.node, 1)).unwrap();
        model.set_output_outlets(&[neg]).unwrap();
        let (rx, tx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            rx.send(model.eval_order()).unwrap();
        });
        assert!(tx.recv_timeout(std::time::Duration::from_secs(1)).unwrap().is_err());
    }
}
