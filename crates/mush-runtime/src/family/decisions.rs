use std::sync::Arc;

use super::{ChildId, ChildStatus, Family, Identity};
use crate::connection::ClientCommand;
use crate::decisions::{Ask, Asked, Asker, DecisionKey};

pub enum AnswerTarget {
    Root(String),
    Child { id: ChildId, text: String },
}

impl Family {
    #[must_use]
    pub fn decisions(&self) -> &Arc<crate::decisions::Decisions> {
        &self.decisions
    }

    pub fn asker(&self, identity: &Identity) -> Result<Asker, String> {
        let inner = self.lock();
        let Some(id) = identity.id.as_deref() else {
            return Ok(Asker {
                session: inner.root_session_id.clone(),
                child: None,
                name: inner.root.name.clone(),
                depth: 0,
                path: Vec::new(),
                ancestors: Vec::new(),
            });
        };
        let node = inner
            .children
            .get(id)
            .ok_or_else(|| format!("{id} is not in the family"))?;
        let mut path = Vec::new();
        let mut ancestors = Vec::new();
        let mut parent = node.summary.parent.clone();
        while let Some(parent_id) = parent {
            let Some(above) = inner.children.get(&parent_id) else {
                break;
            };
            path.push(above.summary.name.clone());
            ancestors.push(above.summary.session_id.clone());
            parent = above.summary.parent.clone();
        }
        ancestors.push(inner.root_session_id.clone());
        path.reverse();
        ancestors.reverse();
        Ok(Asker {
            session: node.summary.session_id.clone(),
            child: Some(id.to_owned()),
            name: node.summary.name.clone(),
            depth: node.summary.depth,
            path,
            ancestors,
        })
    }

    pub fn ask_user(&self, identity: &Identity, ask: Ask) -> Result<Asked, String> {
        let asker = self.asker(identity)?;
        let asked = self.decisions.ask(asker, ask)?;
        if !asked.updated
            && let Some(parent) = asked.decision.asker.parent_session()
        {
            self.decisions
                .post_news(parent, crate::decisions::asked_line(&asked.decision));
        }
        Ok(asked)
    }

    #[must_use]
    pub fn answer_target(&self, asker: &Asker, text: String) -> AnswerTarget {
        let inner = self.lock();
        let mut target = asker.child.clone();
        let mut rerouted = false;
        while let Some(id) = target.clone() {
            match inner.children.get(&id) {
                Some(node) if node.summary.status != ChildStatus::Deleted => break,
                Some(node) => {
                    target = node.summary.parent.clone();
                    rerouted = true;
                }
                None => {
                    target = None;
                    rerouted = true;
                }
            }
        }
        let text = if rerouted {
            format!(
                "These answers were for {}, which is no longer running.\n\n{text}",
                asker.label()
            )
        } else {
            text
        };
        match target {
            Some(id) => AnswerTarget::Child { id, text },
            None => AnswerTarget::Root(text),
        }
    }

    pub fn deliver_to_child(&self, id: ChildId, text: String, keys: Vec<DecisionKey>) {
        let decisions = Arc::clone(&self.decisions);
        let (Some(this), Ok(runtime)) = (self.me.upgrade(), tokio::runtime::Handle::try_current())
        else {
            return;
        };
        decisions.mark_delivered(&keys, true);
        runtime.spawn(async move {
            if let Err(error) = this.prompt_child(&id, text).await {
                tracing::warn!(%id, %error, "decision answers wait for the agent's next turn");
                decisions.mark_delivered(&keys, false);
            }
        });
    }

    async fn prompt_child(self: &Arc<Self>, id: &str, text: String) -> Result<(), String> {
        if self.asleep(id) {
            self.wake(id, self).await?;
        }
        let (control, paused) = {
            let inner = self.lock();
            let node = inner
                .children
                .get(id)
                .ok_or_else(|| format!("{id} is not in the family"))?;
            (node.control.clone(), node.summary.paused.is_some())
        };
        let control = control.ok_or_else(|| format!("{id} is not running"))?;
        let images = Vec::new();
        let command = if paused {
            ClientCommand::QueuePrompt { text, images }
        } else {
            ClientCommand::Prompt { text, images }
        };
        control
            .send(command)
            .map_err(|_| format!("{id} is not running"))
    }

    pub fn confirm_child(&self, id: &str, allow: bool) -> Result<(), String> {
        let control = self
            .lock()
            .children
            .get(id)
            .and_then(|node| node.control.clone())
            .ok_or_else(|| format!("{id} is not running"))?;
        control
            .send(ClientCommand::ConfirmTool { allow })
            .map_err(|_| format!("{id} is not running"))?;
        self.record_update(id, |child| child.awaiting_confirmation = None);
        Ok(())
    }
}
