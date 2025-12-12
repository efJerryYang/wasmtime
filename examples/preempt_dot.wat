(module
  (import "env" "sleep_ms" (func $sleep_ms (param i32)))

  ;; Reserve enough space for 3 x 1024x1024 f64 matrices (~24 MiB). 512 pages = 32 MiB.
  (memory (export "memory") 512)

  ;; Matrix multiply: C = A * B (row-major f64).
  (func $matmul_f64
    (param $ptr_a i32) (param $ptr_b i32) (param $ptr_c i32)
    (param $rows_a i32) (param $cols_a i32) (param $cols_b i32)

    (local $i i32)
    (local $j i32)
    (local $k i32)
    (local $idx i32)
    (local $offset_a i32)
    (local $offset_b i32)
    (local $sum f64)

    (local.set $i (i32.const 0))

    (block $outer_break
      (loop $outer
        (br_if $outer_break
          (i32.ge_u (local.get $i) (local.get $rows_a))
        )

        (local.set $j (i32.const 0))

        (block $mid_break
          (loop $mid
            (br_if $mid_break
              (i32.ge_u (local.get $j) (local.get $cols_b))
            )

            (local.set $sum (f64.const 0))
            (local.set $k (i32.const 0))

            (block $inner_break
              (loop $inner
                (br_if $inner_break
                  (i32.ge_u (local.get $k) (local.get $cols_a))
                )

                ;; A index: (i * cols_a + k)
                (local.set $idx
                  (i32.add
                    (i32.mul (local.get $i) (local.get $cols_a))
                    (local.get $k)
                  )
                )
                (local.set $offset_a
                  (i32.add
                    (local.get $ptr_a)
                    (i32.mul (local.get $idx) (i32.const 8))
                  )
                )

                ;; B index: (k * cols_b + j)
                (local.set $idx
                  (i32.add
                    (i32.mul (local.get $k) (local.get $cols_b))
                    (local.get $j)
                  )
                )
                (local.set $offset_b
                  (i32.add
                    (local.get $ptr_b)
                    (i32.mul (local.get $idx) (i32.const 8))
                  )
                )

                (local.set $sum
                  (f64.add
                    (local.get $sum)
                    (f64.mul
                      (f64.load (local.get $offset_a))
                      (f64.load (local.get $offset_b))
                    )
                  )
                )

                (local.set $k
                  (i32.add (local.get $k) (i32.const 1))
                )
                (br $inner)
              )
            )

            ;; C index: (i * cols_b + j)
            (local.set $idx
              (i32.add
                (i32.mul (local.get $i) (local.get $cols_b))
                (local.get $j)
              )
            )
            (f64.store
              (i32.add
                (local.get $ptr_c)
                (i32.mul (local.get $idx) (i32.const 8))
              )
              (local.get $sum)
            )

            (local.set $j
              (i32.add (local.get $j) (i32.const 1))
            )
            (br $mid)
          )
        )

        (local.set $i
          (i32.add (local.get $i) (i32.const 1))
        )
        (br $outer)
      )
    )
  )

  ;; Looping entrypoint to keep preemption exercised.
  (func (export "matmul_loop")
    (param $ptr_a i32) (param $ptr_b i32) (param $ptr_c i32)
    (param $rows_a i32) (param $cols_a i32) (param $cols_b i32)
    (loop $loop
      (call $matmul_f64
        (local.get $ptr_a) (local.get $ptr_b) (local.get $ptr_c)
        (local.get $rows_a) (local.get $cols_a) (local.get $cols_b))
      (br $loop)
    )
  )

  ;; A simple loop that sleeps for the given milliseconds each iteration.
  (func (export "sleep_loop") (param $ms i32)
    (loop $loop
      (call $sleep_ms (local.get $ms))
      (br $loop)
    )
  )
)
