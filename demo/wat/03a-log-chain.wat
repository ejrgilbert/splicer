;; 03a-log-chain.wat
;;
;; A two-node log chain used as the composition graph for Phase 3 of the
;; splicer demo.
;;
;; Topology:
;;   host(wasi:logging/log@0.1.0)
;;     └─→ $log-provider   (imports from host, passes through the interface)
;;           └─→ $app      (imports from $log-provider, re-exports)
;;                 └─→ export(wasi:logging/log@0.1.0)
;;
;; Interface:  log(level: u8, message: string)
(component
    (import "wasi:logging/log@0.1.0" (instance $log-host
        (export "log" (func (param "level" u8) (param "message" string)))
    ))

    (component $log-provider
        (import "wasi:logging/log@0.1.0" (instance $downstream
            (export "log" (func (param "level" u8) (param "message" string)))
        ))
        (alias export $downstream "log" (func $f))
        (instance $out (export "log" (func $f)))
        (export "wasi:logging/log@0.1.0" (instance $out))
    )

    (instance $log-provider (instantiate $log-provider
        (with "wasi:logging/log@0.1.0" (instance $log-host))
    ))
    (alias export $log-provider "wasi:logging/log@0.1.0" (instance $log-provider-out))

    (component $app
        (import "wasi:logging/log@0.1.0" (instance $log-in
            (export "log" (func (param "level" u8) (param "message" string)))
        ))
        (alias export $log-in "log" (func $f))
        (instance $out (export "log" (func $f)))
        (export "wasi:logging/log@0.1.0" (instance $out))
    )

    (instance $app (instantiate $app
        (with "wasi:logging/log@0.1.0" (instance $log-provider-out))
    ))
    (alias export $app "wasi:logging/log@0.1.0" (instance $app-out))

    (export "wasi:logging/log@0.1.0" (instance $app-out))
)
