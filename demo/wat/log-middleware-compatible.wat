;; Compatible middleware for wasi:logging/log@0.1.0.
;;
;; Passes the imported instance straight through.  The exported interface
;; carries the same `log(level: u32, message: string)` signature as the chain
;; component, so fingerprints will match and validate_contract returns Ok.
(component
    (import "wasi:logging/log@0.1.0" (instance $handler
        (export "log" (func (param "level" u32) (param "message" string)))
    ))
    (export "wasi:logging/log@0.1.0" (instance $handler))
)
