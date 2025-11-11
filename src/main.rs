struct Engine {
    command_queue: Vec<String>,
}

// Key trick: make do_something return a builder that can be delayed
struct DoSomethingCall<'a> {
    engine: &'a mut Engine,
    action: String,
    executed: bool,
}

impl<'a> DoSomethingCall<'a> {
    // Call this to delay execution
    pub fn delay(mut self) {
        self.executed = true;
        println!("Queued: {}", self.action);
        self.engine.command_queue.push(self.action.clone());
    }
}

impl<'a> Drop for DoSomethingCall<'a> {
    fn drop(&mut self) {
        if !self.executed {
            // Immediate execution when no Delayed was passed
            println!("Executing immediately: {}", self.action);
        }
    }
}

impl Engine {
    fn new() -> Self {
        Self {
            command_queue: vec![],
        }
    }

    fn execute_commands(&mut self) {
        for a in self.command_queue.drain(..) {
            println!("Flushed execution: {}", a);
        }
    }

    fn do_something<'a>(&'a mut self, action: &str) -> DoSomethingCall<'a> {
        DoSomethingCall {
            engine: self,
            action: action.to_string(),
            executed: false,
        }
    }
}

fn main() {
    let mut e = Engine::new();

    e.do_something("A").delay(); // delayed (requires explicit execute_commands call)
    e.do_something("B"); // immediate (command executed immediately thanks to Drop trait (executed at semicolon))
    e.do_something("C").delay(); // delayed (requires explicit execute_commands call)
    e.execute_commands();
}
