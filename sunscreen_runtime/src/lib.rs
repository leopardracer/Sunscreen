#![warn(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! This crate contains the types and functions for executing a Sunscreen circuit
//! (i.e. an [`IntermediateRepresentation`](sunscreen_ir::IntermediateRepresentation)).

use sunscreen_ir::{IntermediateRepresentation, Operation::*};

use crossbeam::atomic::AtomicCell;
use petgraph::{stable_graph::NodeIndex, Direction};
use seal::{Ciphertext, Evaluator, RelinearizationKeys};

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/**
 * Run the given [`IntermediateRepresentation`] to completion with the given inputs. This
 * method performs no validation. You must verify the program is first valid. Programs produced
 * by the compiler are guaranteed to be valid, but deserialization does not make any such
 * guarantees. Call [`validate()`](sunscreen_ir::IntermediateRepresentation::validate()) to verify a program's correctness.
 *
 * # Panics
 * Calling this method on a malformed [`IntermediateRepresentation`] may
 * result in a panic.
 *
 * # Non-termination
 * Calling this method on a malformed [`IntermediateRepresentation`] may
 * result in non-termination.
 *
 * # Undefined behavior
 * Calling this method on a malformed [`IntermediateRepresentation`] may
 * result in undefined behavior.
 */
pub unsafe fn run_program_unchecked<E: Evaluator + Sync + Send>(
    ir: &IntermediateRepresentation,
    inputs: &[Ciphertext],
    evaluator: &E,
    relin_keys: Option<RelinearizationKeys>,
) -> Vec<Ciphertext> {
    fn get_ciphertext<'a>(
        data: &'a [AtomicCell<Option<Cow<Ciphertext>>>],
        index: usize,
    ) -> &'a Cow<'a, Ciphertext> {
        // This is correct so long as the IR program is indeed a DAG executed in topological order
        // Since for a given edge (x,y), x executes before y, the operand data that y needs
        // from x will exist.
        let val = unsafe { data[index].as_ptr().as_ref().unwrap() };

        let val = match val {
            Some(v) => v,
            None => panic!("Internal error: No ciphertext found for node {}", index),
        };

        val
    }

    let mut data: Vec<AtomicCell<Option<Cow<Ciphertext>>>> =
        Vec::with_capacity(ir.graph.node_count());

    for _ in 0..ir.graph.node_count() {
        data.push(AtomicCell::new(None));
    }

    parallel_traverse(
        ir,
        |index| {
            let node = &ir.graph[index];

            match &node.operation {
                InputCiphertext(id) => {
                    data[*id].store(Some(Cow::Borrowed(&inputs[*id]))); // moo
                }
                ShiftLeft => unimplemented!(),
                ShiftRight => unimplemented!(),
                Add(a_id, b_id) => {
                    let a = get_ciphertext(&data, a_id.index());
                    let b = get_ciphertext(&data, b_id.index());

                    let c = evaluator.add(&a, &b).unwrap();

                    data[index.index()].store(Some(Cow::Owned(c)));
                }
                Multiply(a_id, b_id) => {
                    let a = get_ciphertext(&data, a_id.index());
                    let b = get_ciphertext(&data, b_id.index());

                    let c = evaluator.multiply(&a, &b).unwrap();

                    data[index.index()].store(Some(Cow::Owned(c)));
                }
                SwapRows => unimplemented!(),
                Relinearize(a_id) => {
                    let relin_keys = relin_keys.as_ref().expect(
                        "Fatal error: attempted to relinearize without relinearization keys.",
                    );

                    let a = get_ciphertext(&data, a_id.index());

                    let c = evaluator.relinearize(&a, relin_keys).unwrap();

                    data[index.index()].store(Some(Cow::Owned(c)));
                }
                Negate => unimplemented!(),
                Sub => unimplemented!(),
                Literal(_x) => unimplemented!(),
                OutputCiphertext(a_id) => {
                    let a = get_ciphertext(&data, a_id.index());

                    data[index.index()].store(Some(Cow::Borrowed(&a)));
                }
            };
        },
        None,
    );

    // Copy ciphertexts to output vector
    ir.graph
        .node_indices()
        .filter_map(|id| match ir.graph[id].operation {
            OutputCiphertext(o_id) => {
                Some(get_ciphertext(&data, o_id.index()).clone().into_owned())
            }
            _ => None,
        })
        .collect()
}

fn parallel_traverse<F>(ir: &IntermediateRepresentation, callback: F, run_to: Option<NodeIndex>)
where
    F: Fn(NodeIndex) -> () + Sync + Send,
{
    let ir = if let Some(x) = run_to {
        Cow::Owned(ir.prune(&vec![x])) // MOO
    } else {
        Cow::Borrowed(ir) // moo
    };

    // Initialize the number of incomplete dependencies.
    let mut deps: HashMap<NodeIndex, AtomicUsize> = HashMap::new();

    for n in ir.graph.node_indices() {
        deps.insert(
            n,
            AtomicUsize::new(ir.graph.neighbors_directed(n, Direction::Outgoing).count()),
        );
    }

    let mut threadpool = scoped_threadpool::Pool::new(num_cpus::get() as u32);
    let items_remaining = AtomicUsize::new(ir.graph.node_count());

    let (sender, reciever) = crossbeam::channel::unbounded();

    for r in deps
        .iter()
        .filter(|(_, count)| count.load(Ordering::Relaxed) == 0)
        .map(|(id, _)| id)
    {
        sender.send(*r).unwrap();
    }

    threadpool.scoped(|scope| {
        for _ in 0..num_cpus::get() {
            scope.execute(|| {
                loop {
                    let mut updated_count = false;

                    // Atomically check if the number of items remaining is zero. If it is,
                    // there's no more work to do, so return. Otherwise, decrement the count
                    // and this thread will take an item.
                    while updated_count {
                        let count = items_remaining.load(Ordering::Acquire);

                        if count == 0 {
                            return;
                        }

                        match items_remaining.compare_exchange_weak(
                            count,
                            count - 1,
                            Ordering::Release,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => {
                                updated_count = true;
                            }
                            _ => {}
                        }
                    }

                    let node_id = reciever.recv().unwrap();

                    callback(node_id);

                    // Check each child's dependency count and mark it as ready if 0.
                    for e in ir.graph.neighbors_directed(node_id, Direction::Outgoing) {
                        let old_val = deps[&e].fetch_sub(1, Ordering::Relaxed);

                        // Note is the value prior to atomic subtraction.
                        if old_val == 1 {
                            sender.send(e).unwrap();
                        }
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use seal::*;

    fn setup_scheme() -> (
        KeyGenerator,
        PublicKey,
        SecretKey,
        Encryptor,
        Decryptor,
        BFVEvaluator,
    ) {
        let degree = 1024;

        let params = BfvEncryptionParametersBuilder::new()
            .set_poly_modulus_degree(degree)
            .set_plain_modulus_u64(100)
            .set_coefficient_modulus(
                CoefficientModulus::bfv_default(degree, SecurityLevel::default()).unwrap(),
            )
            .build()
            .unwrap();

        let context = Context::new(&params, false, SecurityLevel::default()).unwrap();

        let keygen = KeyGenerator::new(&context).unwrap();
        let public_key = keygen.create_public_key();
        let secret_key = keygen.secret_key();

        let encryptor =
            Encryptor::with_public_and_secret_key(&context, &public_key, &secret_key).unwrap();
        let decryptor = Decryptor::new(&context, &secret_key).unwrap();

        let evaluator = BFVEvaluator::new(&context).unwrap();

        (
            keygen, public_key, secret_key, encryptor, decryptor, evaluator,
        )
    }

    #[test]
    fn simple_add() {
        let mut ir = IntermediateRepresentation::new();

        let a = ir.append_input_ciphertext(0);
        let b = ir.append_input_ciphertext(0);
        let c = ir.append_add(a, b);
        ir.append_output_ciphertext(c);

        let (keygen, public_key, secret_key, encryptor, decryptor, evaluator) = setup_scheme();

        let encoder = BFVScalarEncoder::new();
        let pt_0 = encoder.encode_signed(-14).unwrap();
        let pt_1 = encoder.encode_signed(16).unwrap();

        let ct_0 = encryptor.encrypt(&pt_0).unwrap();
        let ct_1 = encryptor.encrypt(&pt_1).unwrap();

        unsafe {
            run_program_unchecked(&ir, &[ct_0, ct_1], &evaluator, None);
        }
    }
}