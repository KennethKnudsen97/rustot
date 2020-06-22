use super::{
    DescribeJobExecutionRequest, DescribeJobExecutionResponse, ErrorResponse,
    GetPendingJobExecutionsRequest, IotJobsData, JobError, JobExecution, JobNotification,
    JobStatus, JobTopicType, NextJobExecutionChanged, StartNextPendingJobExecutionRequest,
    UpdateJobExecutionRequest, UpdateJobExecutionResponse,
};
use crate::consts::{MaxClientTokenLen, MaxTopicLen};
use heapless::{consts, String, Vec};

use serde_json_core::{from_slice, to_vec};

#[derive(Default)]
pub struct JobAgent {
    request_cnt: u32,
    active_job: Option<JobNotification>,
}

pub fn is_job_message(topic_name: &str) -> bool {
    let topic_tokens = topic_name.splitn(8, '/').collect::<Vec<&str, consts::U8>>();
    topic_tokens.get(0) == Some(&"$aws")
        && topic_tokens.get(1) == Some(&"things")
        && topic_tokens.get(3) == Some(&"jobs")
}

impl JobAgent {
    /// Create a new IoT Job Agent reacting to topics for `thing_name`
    pub fn new() -> Self {
        JobAgent {
            request_cnt: 0,
            active_job: None,
        }
    }

    /// Obtains a unique client token on the form `{requestNumber}:{thingName}`,
    /// and increments the request counter
    fn get_client_token(
        &mut self,
        thing_name: &str,
    ) -> Result<String<MaxClientTokenLen>, JobError> {
        let mut client_token = String::new();
        ufmt::uwrite!(&mut client_token, "{}:{}", self.request_cnt, thing_name)
            .map_err(|_| JobError::Formatting)?;
        self.request_cnt += 1;
        Ok(client_token)
    }

    fn update_job_execution_internal<P: mqttrust::PublishPayload>(
        &mut self,
        client: &impl mqttrust::Mqtt<P>,
        execution_number: Option<i64>,
        step_timeout_in_minutes: Option<i64>,
    ) -> Result<(), JobError> {
        let thing_name = client.client_id();
        let client_token = self.get_client_token(thing_name)?;

        if let Some(ref mut active_job) = self.active_job {
            let mut topic = String::new();
            ufmt::uwrite!(
                &mut topic,
                "$aws/things/{}/jobs/{}/update",
                thing_name,
                active_job.job_id.as_str()
            )
            .map_err(|_| JobError::Formatting)?;

            // Always include job_document, and job_execution_state!
            client
                .publish(
                    topic,
                    P::from_bytes(&to_vec::<consts::U512, _>(&UpdateJobExecutionRequest {
                        execution_number,
                        expected_version: active_job.version_number,
                        include_job_document: Some(true),
                        include_job_execution_state: Some(true),
                        status: active_job.status.clone(),
                        step_timeout_in_minutes,
                        client_token,
                    })?),
                    mqttrust::QoS::AtLeastOnce,
                )
                .map_err(|_| JobError::Mqtt)?;

            active_job.version_number += 1;

            Ok(())
        } else {
            Err(JobError::NoActiveJob)
        }
    }

    fn handle_job_execution<P: mqttrust::PublishPayload>(
        &mut self,
        client: &impl mqttrust::Mqtt<P>,
        execution: JobExecution,
    ) -> Result<Option<JobNotification>, JobError> {
        log::info!("Handle job exec! {:?}", execution.status);
        match execution.status {
            JobStatus::Queued if self.active_job.is_none() && execution.job_document.is_some() => {
                // There is a new queued job available, and we are not currently
                // processing a job. Update the status to InProgress, and set it
                // active in the accepted response
                // (`$aws/things/{thingName}/jobs/{jobId}/update/accepted`).
                log::info!("Woot1!");
                self.active_job = Some(JobNotification {
                    job_id: execution.job_id,
                    version_number: execution.version_number,
                    status: JobStatus::InProgress,
                    details: execution.job_document.unwrap(),
                });
                log::info!("Woot!");

                self.update_job_execution_internal(client, None, None)?;
                log::info!("Woot2!");

                Ok(None)
            }
            JobStatus::InProgress
                if self.active_job.is_none() && execution.job_document.is_some() =>
            {
                // If we dont have an active job, and the cloud reports job
                // should be active, it means something panicked, or we lost
                // track of the current job.
                // TODO: Start over on this job, instead of failing it!
                self.active_job = Some(JobNotification {
                    job_id: execution.job_id,
                    version_number: execution.version_number,
                    status: JobStatus::Failed,
                    details: execution.job_document.unwrap(),
                });
                self.update_job_execution_internal(client, None, None)?;
                Ok(None)
            }
            JobStatus::InProgress if self.active_job.is_some() => {
                // If we have an active job, and the cloud reports job should be
                // active, it means there is an update for the currently
                // executing job, perhaps requested by the device

                // TODO:
                // Validate that the job update is indeed for the active_job
                // if execution.job_id == self.active_job {
                //     self.active_job = Some(JobNotification {
                //         ..self.active_job.clone().unwrap()
                //     });
                // }

                Ok(self.active_job.clone())
            }
            JobStatus::Canceled | JobStatus::Removed if self.active_job.is_some() => {
                // Current job is canceled! Abort if possible
                let job = self.active_job.clone().unwrap();
                self.active_job = None;

                Ok(Some(JobNotification {
                    status: JobStatus::Canceled,
                    ..job
                }))
            }
            _ => Ok(None),
        }
    }
}

impl IotJobsData for JobAgent {
    fn describe_job_execution<P: mqttrust::PublishPayload>(
        &mut self,
        client: &impl mqttrust::Mqtt<P>,
        job_id: &str,
        execution_number: Option<i64>,
        include_job_document: Option<bool>,
    ) -> Result<(), JobError> {
        let thing_name = client.client_id();

        let mut topic = String::new();
        ufmt::uwrite!(&mut topic, "$aws/things/{}/jobs/{}/get", thing_name, job_id)
            .map_err(|_| JobError::Formatting)?;

        // TODO: This should be possible to optimize, wrt. clones/copies and allocations
        let p = to_vec::<consts::U512, _>(&DescribeJobExecutionRequest {
            execution_number,
            include_job_document,
            client_token: self.get_client_token(thing_name)?,
        })?;

        client
            .publish(topic, P::from_bytes(&p), mqttrust::QoS::AtLeastOnce)
            .map_err(|_| JobError::Mqtt)?;

        Ok(())
    }

    fn get_pending_job_executions<P: mqttrust::PublishPayload>(
        &mut self,
        client: &impl mqttrust::Mqtt<P>,
    ) -> Result<(), JobError> {
        let thing_name = client.client_id();

        let mut topic = String::new();
        ufmt::uwrite!(&mut topic, "$aws/things/{}/jobs/get", thing_name)
            .map_err(|_| JobError::Formatting)?;

        client
            .publish(
                topic,
                P::from_bytes(&to_vec::<consts::U512, _>(
                    &GetPendingJobExecutionsRequest {
                        client_token: self.get_client_token(thing_name)?,
                    },
                )?),
                mqttrust::QoS::AtLeastOnce,
            )
            .map_err(|_| JobError::Mqtt)?;

        Ok(())
    }

    fn start_next_pending_job_execution<P: mqttrust::PublishPayload>(
        &mut self,
        client: &impl mqttrust::Mqtt<P>,
        step_timeout_in_minutes: Option<i64>,
    ) -> Result<(), JobError> {
        let thing_name = client.client_id();

        let mut topic = String::new();
        ufmt::uwrite!(&mut topic, "$aws/things/{}/jobs/start-next", thing_name)
            .map_err(|_| JobError::Formatting)?;

        client
            .publish(
                topic,
                P::from_bytes(&to_vec::<consts::U512, _>(
                    &StartNextPendingJobExecutionRequest {
                        step_timeout_in_minutes,
                        client_token: self.get_client_token(thing_name)?,
                    },
                )?),
                mqttrust::QoS::AtLeastOnce,
            )
            .map_err(|_| JobError::Mqtt)?;

        Ok(())
    }

    fn update_job_execution<P: mqttrust::PublishPayload>(
        &mut self,
        client: &impl mqttrust::Mqtt<P>,
        status: JobStatus,
    ) -> Result<(), JobError> {
        if let Some(ref mut active_job) = self.active_job {
            active_job.status = status;
        }
        self.update_job_execution_internal(client, None, None)
    }

    fn subscribe_to_jobs<P: mqttrust::PublishPayload>(
        &mut self,
        client: &impl mqttrust::Mqtt<P>,
    ) -> Result<(), JobError> {
        let thing_name = client.client_id();
        let mut topics: Vec<mqttrust::SubscribeTopic, consts::U5> = Vec::new();

        let mut topic = String::new();
        ufmt::uwrite!(&mut topic, "$aws/things/{}/jobs/+/get/+", thing_name)
            .map_err(|_| JobError::Formatting)?;

        topics
            .push(mqttrust::SubscribeTopic {
                topic_path: topic,
                qos: mqttrust::QoS::AtLeastOnce,
            })
            .map_err(|_| JobError::Memory)?;

        let mut topic = String::new();
        ufmt::uwrite!(&mut topic, "$aws/things/{}/jobs/+/update/+", thing_name)
            .map_err(|_| JobError::Formatting)?;

        topics
            .push(mqttrust::SubscribeTopic {
                topic_path: topic,
                qos: mqttrust::QoS::AtLeastOnce,
            })
            .map_err(|_| JobError::Memory)?;

        let mut topic = String::new();
        ufmt::uwrite!(&mut topic, "$aws/things/{}/jobs/notify-next", thing_name)
            .map_err(|_| JobError::Formatting)?;

        topics
            .push(mqttrust::SubscribeTopic {
                topic_path: topic,
                qos: mqttrust::QoS::AtLeastOnce,
            })
            .map_err(|_| JobError::Memory)?;

        client.subscribe(topics).map_err(|_| JobError::Mqtt)?;
        Ok(())
    }

    fn unsubscribe_from_jobs<P: mqttrust::PublishPayload>(
        &mut self,
        client: &impl mqttrust::Mqtt<P>,
    ) -> Result<(), JobError> {
        let thing_name = client.client_id();

        let mut topics: Vec<String<MaxTopicLen>, consts::U5> = Vec::new();

        let mut topic = String::new();
        ufmt::uwrite!(&mut topic, "$aws/things/{}/jobs/+/get/+", thing_name)
            .map_err(|_| JobError::Formatting)?;

        topics.push(topic).map_err(|_| JobError::Memory)?;

        let mut topic = String::new();
        ufmt::uwrite!(&mut topic, "$aws/things/{}/jobs/+/update/+", thing_name)
            .map_err(|_| JobError::Formatting)?;

        topics.push(topic).map_err(|_| JobError::Memory)?;

        let mut topic = String::new();
        ufmt::uwrite!(&mut topic, "$aws/things/{}/jobs/notify-next", thing_name)
            .map_err(|_| JobError::Formatting)?;

        topics.push(topic).map_err(|_| JobError::Memory)?;

        client.unsubscribe(topics).map_err(|_| JobError::Mqtt)?;
        Ok(())
    }

    fn handle_message<P: mqttrust::PublishPayload>(
        &mut self,
        client: &impl mqttrust::Mqtt<P>,
        publish: &mqttrust::PublishNotification,
    ) -> Result<Option<JobNotification>, JobError> {
        match JobTopicType::check(
            client.client_id(),
            &publish
                .topic_name
                // Use the first 7
                // ($aws/things/{thingName}/jobs/$next/get/accepted), leaving
                // tokens 8+ at index 7
                .splitn(8, '/')
                .collect::<Vec<&str, consts::U8>>(),
        ) {
            None => {
                log::debug!("Not a job message!");
                Ok(None)
            }
            Some(JobTopicType::NotifyNext) => {
                // Message published to
                // `$aws/things/{thingName}/jobs/notify-next`

                let response: NextJobExecutionChanged = from_slice(&publish.payload)?;
                log::debug!("notify-next message! {:?}", response);
                if let Some(execution) = response.execution {
                    // Job updated from the cloud!
                    self.handle_job_execution(client, execution)
                } else {
                    // Queue is empty! `jobs done`
                    Ok(None)
                }
            }
            Some(JobTopicType::Notify) => {
                // Message published to `$aws/things/{thingName}/jobs/notify`
                log::error!("notify message!, currently unhandled! Use notify-next instead");

                Ok(None)
            }
            Some(JobTopicType::GetAccepted(job_id)) => {
                // Message published to
                // `$aws/things/{thingName}/jobs/{jobId}/get/accepted`

                log::debug!("{}/get/accepted message!", job_id);
                if let Ok(response) = from_slice::<DescribeJobExecutionResponse>(&publish.payload) {
                    if let Some(execution) = response.execution {
                        self.handle_job_execution(client, execution)
                    } else {
                        Ok(None)
                    }
                } else {
                    log::error!("Unknown job document!");

                    // TODO: See progress for serde(other) can be tracked at:
                    // https://github.com/serde-rs/serde/issues/912
                    //
                    // Update to rejected with a reason of unknown job document!
                    // self.update_job_execution(client, &execution.job_id,
                    // JobStatus::Rejected, execution.version_number, None, None
                    // )?;

                    Ok(None)
                }
            }
            Some(JobTopicType::UpdateAccepted(job_id)) => {
                // Message published to
                // `$aws/things/{thingName}/jobs/{jobId}/update/accepted`
                log::debug!("{}/update/accepted message!", job_id);

                match from_slice::<UpdateJobExecutionResponse>(&publish.payload) {
                    Ok(UpdateJobExecutionResponse {
                        execution_state,
                        job_document,
                        ..
                    }) if execution_state.is_some() && job_document.is_some() => {
                        let state = execution_state.unwrap();

                        let version_number = if let Some(ref active) = self.active_job {
                            if state.version_number > active.version_number {
                                state.version_number
                            } else {
                                active.version_number
                            }
                        } else {
                            state.version_number
                        };

                        match state.status {
                            JobStatus::Canceled | JobStatus::Removed => {
                                self.active_job = None;
                            }
                            _ => {
                                self.active_job = Some(JobNotification {
                                    job_id,
                                    version_number,
                                    status: state.status,
                                    details: job_document.unwrap(),
                                });
                            }
                        }
                        Ok(self.active_job.clone())
                    }
                    Ok(_) => {
                        // job_execution_state or job_document is missing, should never happen!
                        log::error!(
                            "job_execution_state or job_document is missing, should never happen!"
                        );
                        Ok(None)
                    }
                    Err(_) => Err(JobError::InvalidTopic),
                }
            }
            Some(JobTopicType::GetRejected(job_id)) => {
                // Message published to
                // `$aws/things/{thingName}/jobs/{jobId}/get/rejected`
                log::debug!("{}/get/rejected message!", job_id);
                let error: ErrorResponse = from_slice(&publish.payload)?;
                log::debug!("{:?}", error);
                Err(JobError::Rejected(error))
            }
            Some(JobTopicType::UpdateRejected(job_id)) => {
                // Message published to
                // `$aws/things/{thingName}/jobs/{jobId}/update/rejected`
                log::debug!("{}/update/rejected message!", job_id);
                let error: ErrorResponse = from_slice(&publish.payload)?;
                log::debug!("{:?}", error);
                Err(JobError::Rejected(error))
            }
            Some(JobTopicType::Invalid) => Err(JobError::InvalidTopic),
        }
    }
}