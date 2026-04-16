# How Wasm components enable truly pluggable middleware (and much more). #

If you've been keeping up with the Wasi releases, you may have noticed this little WIT snippet in `wasi:http` in the 3.0 release:
```wit
interface handler {
  /// This function may be called with either an incoming request read from the
  /// network or a request synthesized or forwarded by another component.
  handle: async func(request: request) -> result<response, error-code>;
}
world service {
  export handler;
}
world middleware {
  import handler;
  export handler;
}
```

First, you'll note that all that's required to implement a `service` is exporting the `handler` interface. This makes sense as it exposes the entrypoint to handling some incoming HTTP service request and providing a corresponding response. What's more interesting though is this `middleware` world.

Before digging into the meat of what middlewares are used for, I want to emphasize that while this world is called `middleware`, it could really just be defining a service that relies on some downstream service. It's a service that takes in a request, does some processing on it, passes it to some imported service and then returns the response.

So, thinking of this service architecture:
```
HTTP →
  srv-A (calls B)
← HTTP

HTTP →
  srv-B (responds to A)
← HTTP
```

You could avoid the HTTP communication entirely with this WIT:
```
world srv-b {
    export handler;
}
world srv-a {
    import handler; // imports srv-b
    export handler;
}
```
Now we have _all communication happening in-process_:
```
HTTP →
  srv-A → srv-B
← HTTP
```

This is called service chaining, and it's an architecture that's supported by [Fastly] and [Fermyon]. But note here that at its core, it's really an architecture that's natively enabled by the component model itself. You can take two components and compose them together as long as their imports/exports agree[^1].

[Fastly]: https://www.fastly.com/documentation/guides/concepts/service-chaining/
[Fermyon]: https://www.fermyon.com/blog/protect-rest-apis-with-service-chaining
[^1]: If you want to play around with composing components, take a look at the [`wac`](https://github.com/bytecodealliance/wac) tool.

In fact, you can skip out on the HTTP abstraction entirely if you know the types that each service should handle. Simply write the WIT, implement the "services", then create the composition. The following HTTP service is completely valid!
```
package my:service;

interface adder {
    add:        func(a: s32, b: s32) -> s32;
}
interface messenger {
    get-msg:        func() -> string;
}
interface printer1 {
    print1:     func(msg: string);
}

world srv {
    import adder;
    import messenger;
    import printer1;

    include wasi:http/service@0.3.0-rc-2026-01-06; // exports the wasi:http handler!
}
```

It winds up with the following topology after composition where a single HTTP request hits srv, then it calls the downstream functions of its dependencies to form its HTTP response. I'll be calling this the _fan-in_ topology in the rest of the article:

```
HTTP →
          ┌──▶ adder
          │
  srv   ──┼──▶ messenger
          │
          └──▶ printer1
← HTTP
```

Now that we have a solid understanding of the power of Wasm component composition, let's go back to this `middleware` thing that is presented in the `wasi:http` WIT.

## The HTTP Middleware Pattern ##

```
world middleware {
    import handler;
    export handler;
}
```

As mentioned before, this `middleware` world points us to a rather interesting capability. If you're a software design pattern nerd, what's happening here goes back to the [Chain of Responsibility] pattern. This pattern promotes high modularity for cross-cutting concerns in an application through passing some data along a chain of handlers. Each handler in the chain is programmed to do its specific function on the data and continues passing it along to the next handler.

[Chain of Responsibility]: https://refactoring.guru/design-patterns/chain-of-responsibility

This pattern has been applied in the context of HTTP since it is common to perform similar operations to an HTTP request/response across services such as:
- authentication
- timeouts
- encryption/decryption
- request enrichment
- ...and so on (it's really endless)

For examples of this, take a look at Java's [`HandlerInterceptor`], gRPC's [`Interceptors`], [Go's], [Node's], _and_ [Rust's] middleware, the list goes on. While it's great that popular languages support this pattern, there are some crippling limitations here.

[`HandlerInterceptor`]: https://docs.spring.io/spring-framework/docs/current/javadoc-api/org/springframework/web/servlet/HandlerInterceptor.html
[`Interceptors`]: https://grpc.io/docs/guides/interceptors/
[Go's]: https://drstearns.github.io/tutorials/gomiddleware/
[Node's]: https://www.w3schools.com/nodejs/nodejs_middleware.asp
[Rust's]: https://docs.rs/axum/latest/axum/middleware/index.html

## The Limitations ##

**Limitation #1: Heterogeneity.** Web services are implemented in a diverse set of languages. While having a suite of extendable middlewares that a user can leverage in their own context (such as this [Go middleware suite]), their availability is fragmented across languages. Further, customization of such middlewares (even if they exist across languages) is unmaintainable if it must be done for a high number of languages _especially_ as their underlying implementations diverge.

[Go middleware suite]: https://github.com/grpc-ecosystem/go-grpc-middleware

**Limitation #2: An assumed interface.** These implementations of HTTP handler chains assume an HTTP request/reponse payload. Depending on this request/reponse payload actually simplifies quite a lot for this chain pattern. It means that the function signatures of the entire chain agree! But, what if we have a _fan-in_ topology for a service? Reusing middlewares across such interfaces would be impossible as each middleware would need to be customized to fit the function interface it sits on. Note that this constraint applies _even if it doesn't do anything with the payload_ and simply lets it pass through untouched to the downstream handler (for example, a logging middleware).

**Limitation #3: Opaque binaries.** Configuring middleware chains in a source language produces opaque binaries with assumptions about execution context. There is no way to take that binary and make modifications to it such as:
1. Inject new middleware
2. Swap middleware implementations
3. Modify how a service is executed (service chained vs. a chained subset vs. standalone)

Rather, everything is baked opaquely into a single binary without structural cues that would enable any flexibility.

While simply exposing a `middleware` world in `wasi:http` can _kinda_ help with limitation #1 and #3 above, it loses out on some powerful capabilities that the component model provides us. Capabilities that, in fact, help us overcome all these limitations in the context of HTTP middleware **and** empower new use cases using the same mechanisms.

## Why Wasm Component Interposition is the Answer. ##

Interposition here refers to the classic systems technique of inserting a layer between a caller and callee. This layer intercepts ever call across the boundary without modifying the caller or the callee. They are simply opaque units with a well-defined interface the layer sits on. So, this term "Wasm Component Interposition" refers to inserting a component between a caller and a callee component that intercepts the call.

**Benefit #1: Heterogeneity.** Wasm is a polyglot. It's a bytecode format that many languages can compile to. This means that middlewares implemented in a specific language can be reused across heterogeneous services!

**Benefit #2: Interface adaptation.** All the typing information about the payload being passed between caller / callee are transparent on the component interface. This means that a tool could leverage this information to _adapt_ a middleware to be compatible with the target interface on-the-fly. The middleware just needs to be implemented in a way that also adapts to the interface shape. For example, generating an OpenTelemetry trace doesn't require access to the payload at all while a logging middleware may log some information about the payload. This achieves _truly pluggable middleware_ beyond HTTP.

**Benefit #3: Well-structured composition.** This composition isn't just well-structured, it's also discoverable and transparent to the underlying runtime! This means that as [dynamic engine instrumentation capabilities] arise for Wasm, middleware interposition could be managed by the runtime. Think of the possibilities here! Maybe a middleware implements a platform security policy, if that policy gets updated, the runtime could hotswap the middleware without requiring a redeployment of applications. Maybe some strange behavior is happening in-production due to the data coming in? Have the engine dynamically turn on a recorder that streams the data for a live, local debug session, then turn it off when finished. It's pretty powerful stuff here.

[dynamic engine instrumentation capabilities]: https://dl.acm.org/doi/10.1145/3620666.3651338

**Benefit #4: New use-cases.** Yet another benefit of this approach is that it opens up the realm of new possibilities all using the same mechanisms of this handler chain. These arbitrary components interposed on an interface could **completely virtualize** the interface to where you no longer even need what was originally hidden behind it. They could provide **pluggable tooling** like fuzzers and recorders / replayers that adapt to any arbitrary target interface. In fact, this has been shown as possible by Yan Chen in his [`proxy-component` repo].

[`proxy-component` repo]: https://github.com/chenyan2002/proxy-component/tree/main

## A tool to do exactly this is already in development. ##

That's right, and it's an active research project called [`splicer`].

[`splicer`]: https://github.com/ejrgilbert/splicer

`Splicer` is able to automatically interpose Wasm components on an arbitrary interface. It can target interfaces of a standalone component _or_ any arbitrary Wasm composition. All you have to do is provide a Yaml configuration to the tool and the relevant Wasm components.

Given an arbitrary Wasm component, `splicer` discovers its composition graph and uses the Yaml configuration to plan how to interpose Wasm components into its composition. A user can pass a component that matches the interface's function signature **OR** target a WIT adapter interface depending on the capabilities required by the functionality. Then the `splicer` does the heavy work of adapting the component to match the target interface! Read more about how this can be used in your own use cases [here].

[here]: https://github.com/ejrgilbert/splicer/tree/main/docs/adapter-components.md

There's also an in-depth demo of this tool in a repo called [component-interposition].

[component-interposition]: https://github.com/ejrgilbert/component-interposition

## The Roadmap for `splicer` ##

Right now, the tool only supports adapting middleware components that don't require any reflection on the payload of the interface. Read-only reflection and read-write reflection are going to be tackled next! Once these capabilities are added, reusing middlewares from other languages (mentioned above) will be possible!

| Adapter Type | See function names | See types & data | Modify data | Status        |
|--------------|--------------------|------------------|-------------|---------------|
| passthrough  | yes                | no               | no          | ✅ |
| read         | yes                | yes              | no          | planned       |
| read / write | yes                | yes              | yes         | planned       |

Once these adapter types are all supported, there are plans to:
1. implement middlewares that can be reused across heterogeneous interfaces
2. virtualize interfaces
3. implement developer tooling such as record / replay and fuzzers
