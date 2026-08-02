use rquickjs::context::EvalOptions;
use rquickjs::promise::PromiseState;
use rquickjs::{Context, Error, Function, Persistent, Promise, Runtime, Value};

const MAX_CLONE_BYTES: usize = 1_024;
const MAX_EXCEPTION_MESSAGE_BYTES: usize = 252;
const MAX_EXCEPTION_STACK_BYTES: usize = 32;

#[derive(Debug, Eq, PartialEq)]
enum CloneError {
    Rejected,
    TooLarge,
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }

    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn strict_json_encode(context: &Context, candidate_expression: &str) -> Result<String, CloneError> {
    let encoded = context.with(|ctx| {
        let source = format!(
            r#"
            (() => {{
                const candidate = ({candidate_expression});
                const active = new Set();

                function clone(value) {{
                    if (value === null || typeof value === "string" || typeof value === "boolean") {{
                        return value;
                    }}
                    if (typeof value === "number" && Number.isFinite(value)) {{
                        return value;
                    }}
                    if (typeof value !== "object" || active.has(value)) {{
                        throw new TypeError("value is outside the JSON clone contract");
                    }}

                    active.add(value);
                    let copy;
                    if (Array.isArray(value)) {{
                        copy = [];
                        for (let index = 0; index < value.length; index += 1) {{
                            const descriptor = Object.getOwnPropertyDescriptor(value, String(index));
                            if (!descriptor || !descriptor.enumerable || !("value" in descriptor)) {{
                                throw new TypeError("array elements must be enumerable data properties");
                            }}
                            copy.push(clone(descriptor.value));
                        }}
                        const extraKeys = Reflect.ownKeys(value).filter((key) => {{
                            if (key === "length") return false;
                            if (typeof key !== "string") return true;
                            const index = Number(key);
                            return !Number.isInteger(index) || index < 0 || index >= value.length || String(index) !== key;
                        }});
                        if (extraKeys.length !== 0) {{
                            throw new TypeError("array properties are outside the JSON clone contract");
                        }}
                    }} else {{
                        const prototype = Object.getPrototypeOf(value);
                        if (prototype !== Object.prototype && prototype !== null) {{
                            throw new TypeError("host objects are outside the JSON clone contract");
                        }}
                        copy = Object.create(null);
                        for (const key of Reflect.ownKeys(value)) {{
                            if (typeof key !== "string") {{
                                throw new TypeError("symbol keys are outside the JSON clone contract");
                            }}
                            const descriptor = Object.getOwnPropertyDescriptor(value, key);
                            if (!descriptor.enumerable || !("value" in descriptor)) {{
                                throw new TypeError("accessors are outside the JSON clone contract");
                            }}
                            copy[key] = clone(descriptor.value);
                        }}
                    }}
                    active.delete(value);
                    return copy;
                }}

                return JSON.stringify(clone(candidate));
            }})()
            "#
        );

        match ctx.eval::<String, _>(source) {
            Ok(encoded) => Ok(encoded),
            Err(Error::Exception) => {
                let _ = ctx.catch();
                Err(CloneError::Rejected)
            }
            Err(_) => Err(CloneError::Rejected),
        }
    })?;

    if encoded.len() > MAX_CLONE_BYTES {
        return Err(CloneError::TooLarge);
    }
    Ok(encoded)
}

#[test]
fn skill_function_keeps_its_own_global_realm_when_called_from_agent_context() {
    let runtime = Runtime::new().expect("create QuickJS runtime");
    let skill_context = Context::full(&runtime).expect("create skill context");
    let agent_context = Context::full(&runtime).expect("create agent context");

    let skill_function = skill_context.with(|ctx| {
        ctx.globals()
            .set("realm_sentinel", "skill")
            .expect("set skill sentinel");
        let function: Function = ctx
            .eval("() => globalThis.realm_sentinel")
            .expect("create skill function");
        Persistent::save(&ctx, function)
    });

    agent_context.with(|ctx| {
        ctx.globals()
            .set("realm_sentinel", "agent")
            .expect("set agent sentinel");
        let function = skill_function
            .restore(&ctx)
            .expect("restore skill function in the shared runtime");
        let result: String = function.call(()).expect("call cross-context function");
        assert_eq!(result, "skill");
        assert_eq!(
            ctx.eval::<String, _>("globalThis.realm_sentinel")
                .expect("read agent sentinel"),
            "agent"
        );
    });
}

#[test]
fn skill_promise_settles_while_runtime_jobs_are_drained_from_the_agent_step() {
    let runtime = Runtime::new().expect("create QuickJS runtime");
    let skill_context = Context::full(&runtime).expect("create skill context");
    let agent_context = Context::full(&runtime).expect("create agent context");

    let skill_function = skill_context.with(|ctx| {
        ctx.globals()
            .set("realm_sentinel", "skill-continued")
            .expect("set skill sentinel");
        let function: Function = ctx
            .eval("() => Promise.resolve().then(() => globalThis.realm_sentinel)")
            .expect("create async skill function");
        Persistent::save(&ctx, function)
    });

    let promise = agent_context.with(|ctx| {
        ctx.globals()
            .set("realm_sentinel", "agent")
            .expect("set agent sentinel");
        let function = skill_function
            .restore(&ctx)
            .expect("restore async skill function");
        let promise: Promise = function.call(()).expect("call async skill function");
        assert_eq!(promise.state(), PromiseState::Pending);
        Persistent::save(&ctx, promise)
    });

    assert!(runtime.is_job_pending(), "continuation must enqueue a job");
    assert!(
        matches!(runtime.execute_pending_job(), Ok(true)),
        "the agent step must execute at least one runtime job"
    );

    agent_context.with(|ctx| {
        let promise = promise.restore(&ctx).expect("restore settled promise");
        assert_eq!(promise.state(), PromiseState::Resolved);
        let result = promise
            .result::<String>()
            .expect("resolved promise must have a result")
            .expect("convert resolved promise result");
        assert_eq!(result, "skill-continued");
    });
    assert!(
        !runtime.is_job_pending(),
        "the continuation must fully settle"
    );
}

#[test]
fn cross_context_exceptions_preserve_bounded_message_and_stack() {
    let runtime = Runtime::new().expect("create QuickJS runtime");
    let skill_context = Context::full(&runtime).expect("create skill context");
    let agent_context = Context::full(&runtime).expect("create agent context");

    let throwing_function = skill_context.with(|ctx| {
        let mut options = EvalOptions::default();
        options.filename = Some("skill-realm-isolation.js".to_owned());
        let function: Function = ctx
            .eval_with_options(
                "function throw_from_skill() { throw new Error('realm-boom-💥'.repeat(1000)); } throw_from_skill",
                options,
            )
            .expect("create throwing skill function");
        Persistent::save(&ctx, function)
    });

    agent_context.with(|ctx| {
        let function = throwing_function
            .restore(&ctx)
            .expect("restore throwing skill function");
        assert!(matches!(
            function.call::<_, Value>(()),
            Err(Error::Exception)
        ));

        let thrown = function.ctx().catch();
        let exception = thrown
            .as_exception()
            .expect("skill must throw an Error object");
        let message = exception.message().expect("exception message");
        let stack = exception.stack().expect("exception stack");
        assert!(message.starts_with("realm-boom-💥"));
        assert!(stack.contains("throw_from_skill"), "stack was {stack:?}");
        assert!(
            stack.contains("skill-realm-isolation.js"),
            "stack was {stack:?}"
        );

        let bounded_message = truncate_utf8(&message, MAX_EXCEPTION_MESSAGE_BYTES);
        let bounded_stack = truncate_utf8(&stack, MAX_EXCEPTION_STACK_BYTES);
        assert!(message.len() > MAX_EXCEPTION_MESSAGE_BYTES);
        assert!(stack.len() > MAX_EXCEPTION_STACK_BYTES);
        assert!(bounded_message.len() <= MAX_EXCEPTION_MESSAGE_BYTES);
        assert!(bounded_stack.len() <= MAX_EXCEPTION_STACK_BYTES);
        assert!(bounded_message.is_char_boundary(bounded_message.len()));
        assert!(bounded_stack.is_char_boundary(bounded_stack.len()));
        assert!(message.starts_with(&bounded_message));
        assert!(stack.starts_with(&bounded_stack));
    });
}

#[test]
fn skill_context_cannot_resolve_agent_effect_or_proposal_globals() {
    let runtime = Runtime::new().expect("create QuickJS runtime");
    let skill_context = Context::full(&runtime).expect("create skill context");
    let agent_context = Context::full(&runtime).expect("create agent context");

    const GLOBAL_TYPES: &str = r#"[
        typeof read_file,
        typeof write_file,
        typeof fetch,
        typeof spawn,
        typeof propose_skill
    ].join(",")"#;

    agent_context.with(|ctx| {
        ctx.eval::<(), _>(
            "globalThis.read_file = globalThis.write_file = globalThis.fetch =\
             globalThis.spawn = globalThis.propose_skill = () => 'agent authority'",
        )
        .expect("install agent authority globals");
        assert_eq!(
            ctx.eval::<String, _>(GLOBAL_TYPES)
                .expect("inspect agent globals"),
            "function,function,function,function,function"
        );
    });

    skill_context.with(|ctx| {
        assert_eq!(
            ctx.eval::<String, _>(GLOBAL_TYPES)
                .expect("inspect skill globals"),
            "undefined,undefined,undefined,undefined,undefined"
        );
    });
}

#[test]
fn values_cross_the_boundary_only_through_the_declared_json_clone_contract() {
    let runtime = Runtime::new().expect("create QuickJS runtime");
    let skill_context = Context::full(&runtime).expect("create skill context");
    let agent_context = Context::full(&runtime).expect("create agent context");

    let encoded = strict_json_encode(
        &skill_context,
        r#"({name: "skill", nested: [true, null, 42, {realm: "private"}], ["__proto__"]: {preserved: true}})"#,
    )
    .expect("encode JSON-safe skill value");
    agent_context.with(|ctx| {
        let cloned = ctx.json_parse(encoded).expect("parse clone in agent realm");
        ctx.globals()
            .set("cloned", cloned)
            .expect("publish cloned value");
        assert!(
            ctx.eval::<bool, _>(
                "cloned.name === 'skill' && cloned.nested[3].realm === 'private' &&\
                 Object.prototype.hasOwnProperty.call(cloned, '__proto__') &&\
                 cloned.__proto__.preserved === true &&\
                 Object.getPrototypeOf(cloned) === Object.prototype &&\
                 Object.getPrototypeOf(cloned.nested) === Array.prototype",
            )
            .expect("inspect cloned value")
        );
    });

    for hostile in [
        "() => 1",
        "Symbol('secret')",
        "1n",
        "NaN",
        "Infinity",
        "-Infinity",
        "({nested: undefined})",
        "({nested: () => 1})",
        "({[Symbol('key')]: 'value'})",
        "new Date(0)",
        "Object.defineProperty({}, 'secret', {enumerable: true, get() { throw new Error('getter'); }})",
        "(() => { const value = {}; value.self = value; return value; })()",
        "(() => { const value = []; value.length = 2; value[1] = 'sparse'; return value; })()",
        "(() => { const value = []; value['4294967295'] = 'not-an-index'; return value; })()",
    ] {
        assert_eq!(
            strict_json_encode(&skill_context, hostile),
            Err(CloneError::Rejected),
            "hostile value unexpectedly crossed: {hostile}"
        );
    }

    let array_accessor = r#"(() => {
        globalThis.array_index_getter_calls = 0;
        const value = [];
        Object.defineProperty(value, "0", {
            enumerable: true,
            get() {
                globalThis.array_index_getter_calls += 1;
                return "getter result";
            }
        });
        return value;
    })()"#;
    assert_eq!(
        strict_json_encode(&skill_context, array_accessor),
        Err(CloneError::Rejected)
    );
    skill_context.with(|ctx| {
        assert_eq!(
            ctx.eval::<i32, _>("globalThis.array_index_getter_calls")
                .expect("read array getter canary"),
            0,
            "array index accessors must be rejected without execution"
        );
    });

    let json_string_overhead = strict_json_encode(&skill_context, "''")
        .expect("encode empty string")
        .len();
    let exact_payload_bytes = MAX_CLONE_BYTES
        .checked_sub(json_string_overhead)
        .expect("clone limit must fit an empty JSON string");
    let exact_limit_expression = format!("'x'.repeat({exact_payload_bytes})");
    let exact_limit = strict_json_encode(&skill_context, &exact_limit_expression)
        .expect("an encoding exactly at the clone limit must pass");
    assert_eq!(exact_limit.len(), MAX_CLONE_BYTES);

    let over_limit_expression = format!("'x'.repeat({})", exact_payload_bytes + 1);
    assert_eq!(
        strict_json_encode(&skill_context, &over_limit_expression),
        Err(CloneError::TooLarge)
    );
}
