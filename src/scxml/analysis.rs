use std::collections::BTreeSet;

use super::ast::*;

impl Document {
    /// Return the deterministic set of structural capabilities used by this document.
    pub fn capabilities(&self) -> BTreeSet<Capability> {
        let mut capabilities = BTreeSet::new();
        CapabilityAnalyzer::new(&mut capabilities).visit_scxml(&self.root);
        capabilities
    }
}

struct CapabilityAnalyzer<'a> {
    capabilities: &'a mut BTreeSet<Capability>,
}

impl<'a> CapabilityAnalyzer<'a> {
    fn new(capabilities: &'a mut BTreeSet<Capability>) -> Self {
        Self { capabilities }
    }

    fn add(&mut self, capability: Capability) {
        self.capabilities.insert(capability);
    }

    fn visit_scxml(&mut self, scxml: &Scxml) {
        let explicit_initial = !scxml.initial.is_empty();
        let has_state_children = scxml.children.iter().any(|child| {
            matches!(
                child,
                RootChild::State(_) | RootChild::Parallel(_) | RootChild::Final(_)
            )
        });
        if has_state_children || explicit_initial {
            self.add(Capability::CompoundState);
            if explicit_initial {
                self.add(Capability::ExplicitInitialTransition);
            } else {
                self.add(Capability::DefaultInitialTransition);
            }
        }

        for child in &scxml.children {
            self.visit_root_child(child);
        }
    }

    fn visit_root_child(&mut self, child: &RootChild) {
        match child {
            RootChild::DataModel(value) => self.visit_datamodel(value),
            RootChild::State(value) => self.visit_state(value),
            RootChild::Parallel(value) => self.visit_parallel(value),
            RootChild::Final(value) => self.visit_final(value),
            RootChild::Script(value) => {
                self.add(Capability::Script);
                self.visit_extensions(&value.extensions);
            }
            RootChild::Extension(value) => self.visit_extension(value),
        }
    }

    fn visit_state(&mut self, state: &State) {
        let explicit_initial = !state.initial.is_empty()
            || state
                .children
                .iter()
                .any(|child| matches!(child, StateChild::Initial(_)));
        let has_state_children = state.children.iter().any(|child| {
            matches!(
                child,
                StateChild::State(_) | StateChild::Parallel(_) | StateChild::Final(_)
            )
        });
        if has_state_children || explicit_initial {
            self.add(Capability::CompoundState);
            if explicit_initial {
                self.add(Capability::ExplicitInitialTransition);
            } else {
                self.add(Capability::DefaultInitialTransition);
            }
        }
        self.visit_state_children(&state.children);
    }

    fn visit_parallel(&mut self, parallel: &Parallel) {
        self.add(Capability::ParallelState);
        self.visit_parallel_children(&parallel.children);
    }

    fn visit_parallel_children(&mut self, children: &[ParallelChild]) {
        for child in children {
            match child {
                ParallelChild::OnEntry(value) => self.visit_content(&value.content),
                ParallelChild::OnExit(value) => self.visit_content(&value.content),
                ParallelChild::State(value) => self.visit_state(value),
                ParallelChild::Parallel(value) => self.visit_parallel(value),
                ParallelChild::History(value) => self.visit_history(value),
                ParallelChild::Transition(value) => self.visit_transition(value),
                ParallelChild::DataModel(value) => self.visit_datamodel(value),
                ParallelChild::Extension(value) => self.visit_extension(value),
            }
        }
    }

    fn visit_state_children(&mut self, children: &[StateChild]) {
        for child in children {
            match child {
                StateChild::OnEntry(value) => self.visit_content(&value.content),
                StateChild::OnExit(value) => self.visit_content(&value.content),
                StateChild::Initial(value) => self.visit_initial(value),
                StateChild::State(value) => self.visit_state(value),
                StateChild::Parallel(value) => self.visit_parallel(value),
                StateChild::Final(value) => self.visit_final(value),
                StateChild::History(value) => self.visit_history(value),
                StateChild::Transition(value) => self.visit_transition(value),
                StateChild::DataModel(value) => self.visit_datamodel(value),
                StateChild::Extension(value) => self.visit_extension(value),
            }
        }
    }

    fn visit_final(&mut self, final_state: &Final) {
        self.add(Capability::FinalState);
        for child in &final_state.children {
            match child {
                FinalChild::OnEntry(value) => self.visit_content(&value.content),
                FinalChild::OnExit(value) => self.visit_content(&value.content),
                FinalChild::DoneData(value) => self.visit_donedata(value),
                FinalChild::Extension(value) => self.visit_extension(value),
            }
        }
    }

    fn visit_history(&mut self, history: &History) {
        if history.history_type.as_deref() == Some("deep") {
            self.add(Capability::DeepHistory);
        } else {
            self.add(Capability::ShallowHistory);
        }
        for child in &history.children {
            match child {
                HistoryChild::Transition(value) => self.visit_transition(value),
                HistoryChild::Extension(value) => self.visit_extension(value),
            }
        }
    }

    fn visit_initial(&mut self, initial: &Initial) {
        self.add(Capability::ExplicitInitialTransition);
        if let Some(transition) = &initial.transition {
            self.visit_transition(transition);
        }
        self.visit_extensions(&initial.extensions);
    }

    fn visit_transition(&mut self, transition: &Transition) {
        let event_tokens: Vec<&str> = if transition.events.is_empty() {
            transition
                .event
                .as_deref()
                .into_iter()
                .flat_map(str::split_whitespace)
                .collect()
        } else {
            transition.events.iter().map(String::as_str).collect()
        };
        if transition.after.is_some() {
            self.add(Capability::AfterTransition);
        } else if event_tokens.is_empty() {
            self.add(Capability::EventlessTransition);
        }
        if event_tokens.len() > 1 {
            self.add(Capability::MultiEventTransition);
        }
        for event in event_tokens {
            if event == "*" {
                self.add(Capability::WildcardEventDescriptor);
            } else if event.ends_with(".*") {
                self.add(Capability::PrefixEventDescriptor);
            }
        }
        if transition.targets.is_empty() {
            self.add(Capability::TargetlessTransition);
        } else if transition.targets.len() > 1 {
            self.add(Capability::MultiTargetTransition);
        }
        if transition.condition.is_some() {
            self.add(Capability::Conditions);
        }
        if transition.transition_type == Some(TransitionType::Internal) {
            self.add(Capability::InternalTransition);
        }
        self.visit_content(&transition.content);
    }

    fn visit_datamodel(&mut self, datamodel: &DataModel) {
        self.add(Capability::DataModel);
        for child in &datamodel.children {
            match child {
                DataModelChild::Data(value) => self.add_data(value),
                DataModelChild::Extension(value) => self.visit_extension(value),
            }
        }
    }

    fn add_data(&mut self, data: &Data) {
        self.add(Capability::Data);
        if let Some(content) = &data.content {
            self.visit_payload_content(content);
        }
    }

    fn visit_donedata(&mut self, donedata: &DoneData) {
        for child in &donedata.children {
            match child {
                DoneDataChild::Param(value) => {
                    self.add(Capability::Param);
                    self.visit_extensions(&value.extensions);
                }
                DoneDataChild::Content(value) => self.visit_content_node(value),
                DoneDataChild::Extension(value) => self.visit_extension(value),
            }
        }
    }

    fn visit_content_node(&mut self, content: &Content) {
        self.add(Capability::Content);
        self.visit_payload_content(content);
    }

    fn visit_payload_content(&mut self, content: &Content) {
        self.add(Capability::PayloadContent);
        for part in &content.parts {
            let ContentPart::Element(value) = part else {
                continue;
            };
            self.visit_payload_element(value);
        }
    }

    fn visit_payload_element(&mut self, element: &PayloadElement) {
        for part in &element.content {
            if let PayloadPart::Element(value) = part {
                self.visit_payload_element(value);
            }
        }
    }

    fn visit_content(&mut self, content: &[ExecutableContent]) {
        for executable in content {
            self.visit_executable(executable);
        }
    }

    fn visit_executable(&mut self, executable: &ExecutableContent) {
        match executable {
            ExecutableContent::Raise(_) => self.add(Capability::Raise),
            ExecutableContent::Assign(value) => {
                self.add(Capability::Assign);
                if let Some(content) = &value.content {
                    self.visit_payload_content(content);
                }
            }
            ExecutableContent::If(value) => {
                self.add(Capability::If);
                if value.condition.is_some() {
                    self.add(Capability::Conditions);
                }
                for child in &value.children {
                    match child {
                        IfChild::Executable(value) => self.visit_executable(value),
                        IfChild::ElseIf(value) => {
                            if value.condition.is_some() {
                                self.add(Capability::Conditions);
                            }
                            self.visit_content(&value.content);
                        }
                        IfChild::Else(value) => self.visit_content(&value.content),
                        IfChild::Extension(value) => self.visit_extension(value),
                    }
                }
            }
            ExecutableContent::Foreach(value) => {
                self.add(Capability::Foreach);
                self.visit_content(&value.content);
            }
            ExecutableContent::Log(_) => self.add(Capability::Log),
            ExecutableContent::Send(value) => {
                self.add(Capability::Send);
                for child in &value.children {
                    match child {
                        SendChild::Param(value) => {
                            self.add(Capability::Param);
                            self.visit_extensions(&value.extensions);
                        }
                        SendChild::Content(value) => self.visit_content_node(value),
                        SendChild::Extension(value) => self.visit_extension(value),
                    }
                }
            }
            ExecutableContent::Cancel(_) => self.add(Capability::Cancel),
            ExecutableContent::Script(value) => {
                self.add(Capability::Script);
                self.visit_extensions(&value.extensions);
            }
            ExecutableContent::Extension(value) => self.visit_extension(value),
        }
    }

    fn visit_extensions(&mut self, extensions: &[Extension]) {
        for extension in extensions {
            self.visit_extension(extension);
        }
    }

    fn visit_extension(&mut self, extension: &Extension) {
        if extension.namespace.as_deref() == Some(super::BEVY_STATE_TREE_NAMESPACE)
            && extension.local_name == "effect"
        {
            self.add(Capability::TypedEffect);
            return;
        } else {
            self.add(Capability::ForeignExtension);
        }
        for child in &extension.content {
            if let ExtensionContent::Element(child) = child {
                self.visit_extension(child);
            }
        }
    }
}
