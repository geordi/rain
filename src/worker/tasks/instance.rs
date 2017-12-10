
use futures::Future;

use std::rc::Rc;
use worker::graph::{TaskRef, SubworkerRef, TaskState};
use errors::{Result, Error};
use worker::state::{StateRef, State};
use worker::tasks;
use worker::rpc::subworker::data_from_capnp;
use common::convert::ToCapnp;
use common::Additionals;
use common::wrapped::WrappedRcRefCell;

/// Instance represents a running task. It contains resource allocations and
/// allows to signal finishing of data objects.

pub struct TaskInstance {
    task_ref: TaskRef,
    // TODO resources

    // When this sender is triggered, then task is forcefully terminated
    // When cancel_sender is None, termination is actually running
    cancel_sender: Option<::futures::unsync::oneshot::Sender<()>>,
    //pub subworker: Option<SubworkerRef>
}

pub type TaskFuture = Future<Item = (), Error = Error>;
pub type TaskResult = Result<Box<TaskFuture>>;


fn fail_unknown_type(state: &mut State, task_ref: TaskRef) -> TaskResult {
    bail!("Unknown task type {}", task_ref.get().task_type)
}

impl TaskInstance {

    pub fn start(state: &mut State, task_ref: TaskRef) {
        {
            let mut task = task_ref.get_mut();
            state.alloc_resources(&task.resources);
            task.state = TaskState::Running;
            state.task_updated(&task_ref);
        }

        let task_fn = {
            let task = task_ref.get();
            let task_type : &str = task.task_type.as_ref();
            // Build-in task
            match task_type {
                task_type if !task_type.starts_with("!") => Self::start_task_in_subworker,
                "!run" => tasks::run::task_run,
                "!concat" => tasks::basic::task_concat,
                "!sleep" => tasks::basic::task_sleep,
                "!open" => tasks::basic::task_open,
                _ => fail_unknown_type,
            }
        };

        let future : Box<TaskFuture> = match task_fn(state, task_ref.clone()) {
            Ok(f) => f,
            Err(e) => {
                state.unregister_task(&task_ref);
                let mut task = task_ref.get_mut();
                state.free_resources(&task.resources);
                task.set_failed(e.description().to_string());
                state.task_updated(&task_ref);
                return
            }
        };

        let (sender, receiver) = ::futures::unsync::oneshot::channel::<()>();

        let task_id = task_ref.get().id;
        let instance = TaskInstance {
            task_ref: task_ref,
            cancel_sender: Some(sender),
        };
        let state_ref = state.self_ref();
        state.graph.running_tasks.insert(task_id, instance);

        state.spawn_panic_on_error(future
                                   .map(|()| true)
                                   .select(receiver
                                           .map(|()| false)
                                           .map_err(|_| unreachable!()))
                                   .then(move |r| {
            let mut state = state_ref.get_mut();
            let instance = state.graph.running_tasks.remove(&task_id).unwrap();
            state.task_updated(&instance.task_ref);
            state.unregister_task(&instance.task_ref);
            let mut task = instance.task_ref.get_mut();
            state.free_resources(&task.resources);
            match r {
                Ok((true, _)) => {
                    let all_finished = task.outputs.iter()
                        .all(|o| o.get().is_finished());
                    if !all_finished {
                        task.set_failed("Some of outputs were not produced"
                                        .to_string());
                    } else {
                        for output in &task.outputs {
                            state.object_is_finished(output);
                        }
                        debug!("Task was successfully finished");
                        task.state = TaskState::Finished;
                    }
                },
                Ok((false, _)) => {
                    debug!("Task {} was terminated", task.id);
                    task.set_failed("Task terminated by server".into());
                },
                Err((e, _)) => {
                    task.set_failed(e.description().to_string());
                }
            };
            Ok(())
        }));
    }

    pub fn stop(&mut self) {
        let cancel_sender = ::std::mem::replace(&mut self.cancel_sender, None);
        if let Some(sender) = cancel_sender {
            sender.send(()).unwrap();
        } else {
            debug!("Task stopping is already in progress");
        }
    }

    fn start_task_in_subworker(state: &mut State, task_ref: TaskRef) -> TaskResult {
        let future = state.get_subworker(task_ref.get().task_type.as_ref())?;
        let state_ref = state.self_ref();
        Ok(Box::new(future.and_then(move |subworker| {
            // Run task in subworker
                let mut req = subworker.get().control().run_task_request();
                {
                    let task = task_ref.get();
                    debug!("Starting task id={} in subworker", task.id);
                    // Serialize task
                    let mut param_task = req.get().get_task().unwrap();
                    task.id.to_capnp(&mut param_task.borrow().get_id().unwrap());
                    param_task.set_task_config(&task.task_config);

                    param_task.borrow().init_inputs(task.inputs.len() as u32);
                    {
                        // Serialize inputs of task
                        let mut p_inputs = param_task.borrow().get_inputs().unwrap();
                        for (i, input) in task.inputs.iter().enumerate() {
                            let mut p_input = p_inputs.borrow().get(i as u32);
                            p_input.set_label(&input.label);
                            let obj = input.object.get();
                            obj.data().to_subworker_capnp(
                                &mut p_input.borrow().get_data().unwrap(),
                            );
                            obj.id.to_capnp(&mut p_input.get_id().unwrap());
                        }
                    }


                    param_task.borrow().init_outputs(task.outputs.len() as u32);
                    {
                        // Serialize outputs of task
                        let mut p_outputs = param_task.get_outputs().unwrap();
                        for (i, output) in task.outputs.iter().enumerate() {
                            let mut p_output = p_outputs.borrow().get(i as u32);
                            let obj = output.get();
                            p_output.set_label(&obj.label);
                            obj.id.to_capnp(&mut p_output.get_id().unwrap());
                        }
                    }
                }
            req.send().promise
                .map_err::<_, Error>(|e| e.into())
                .then(move |r| {
                    let result = match r {
                        Ok(response) => {
                            let mut task = task_ref.get_mut();
                            let response = response.get()?;
                            task.new_additionals.update(
                                Additionals::from_capnp(&response.get_task_additionals()?));
                            let subworker = subworker.get();
                            let work_dir = subworker.work_dir();
                            if response.get_ok() {
                                debug!("Task id={} finished in subworker", task.id);
                                for (co, output) in response.get_data()?.iter().zip(&task.outputs) {
                                    let data = data_from_capnp(&state_ref.get(), work_dir, &co)?;
                                    output.get_mut().set_data(data);
                                }
                            } else {
                                debug!("Task id={} failed in subworker", task.id);
                                bail!(response.get_error_message()?);
                            }
                            Ok(())
                        },
                        Err(err) => Err(err.into())
                    };
                    state_ref.get_mut().graph.idle_subworkers.push(subworker);
                    result
                })
        })))
    }
}