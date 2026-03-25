;; 03c-middleware-incompatible.wat
;;
;; A standalone middleware component whose log interface is INCOMPATIBLE with
;; the chain in 03a-log-chain.wat.
;;
;; Exports wasi:logging/log@0.1.0 with a different signature:
;;   log(level: u8, context: string, message: string)   ← extra "context" param
;;
;; The inner $mw-impl component is required so that the outer export resolves
;; to a CompInst (enabling fingerprint extraction by cviz).
(component
    (import "wasi:logging/log@0.1.0" (instance $downstream
        (export "log" (func (param "level" u8) (param "context" string) (param "message" string)))
    ))

    (component $mw-impl
        (import "wasi:logging/log@0.1.0" (instance $log-in
            (export "log" (func (param "level" u8) (param "context" string) (param "message" string)))
        ))
        (alias export $log-in "log" (func $f))
        (instance $out (export "log" (func $f)))
        (export "wasi:logging/log@0.1.0" (instance $out))
    )

    (instance $inst (instantiate $mw-impl
        (with "wasi:logging/log@0.1.0" (instance $downstream))
    ))
    (alias export $inst "wasi:logging/log@0.1.0" (instance $mw-out))

    (export "wasi:logging/log@0.1.0" (instance $mw-out))
)
