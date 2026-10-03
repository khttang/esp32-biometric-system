/*
 * Workarounds for the ESP32-P4's hardware-loop erratum on silicon before v3, in the two places
 * where ESP-IDF v5.5.5's context switch does not cover it. Without them, a task that is
 * preempted in the middle of an ESP-DL inference can resume with a hardware loop that ends one
 * iteration early. For a neural network that is not a crash but a slightly wrong result: here,
 * face embeddings a few quantisation steps off whenever the camera thread preempted the
 * inference thread, while the same run at the highest priority was bit-exact every time.
 *
 * Background. The P4 has "hardware loop" registers (start, end and count for two loops) and the
 * PIE vector extension; ESP-DL's assembly kernels use both, with PIE instructions inside
 * hardware loops. FreeRTOS treats both as coprocessors:
 *
 *   - hardware-loop registers are saved at interrupt entry if the CPU reports their state as
 *     DIRTY, and restored when the task is scheduled again;
 *   - PIE is switched off at interrupt entry. If the same task continues, it is switched back
 *     on; after a context switch it stays off, and the task's next PIE instruction traps so that
 *     the handler can save and restore the vector registers lazily.
 *
 * Both routines below are attached with the linker's `--wrap` option
 * (firmware/.cargo/config.toml), so ESP-IDF itself is not modified. Remove this file and those
 * options once the ESP-IDF version in use handles both cases.
 */

#include "sdkconfig.h"
#include "soc/soc_caps.h"
#include "riscv/csr_hwlp.h"
#include "riscv/csr_pie.h"
#include "riscv/rvruntime-frames.h"

#if SOC_CPU_HAS_HWLOOP && SOC_CPU_HAS_HWLOOP_STATE_BUG && SOC_CPU_HAS_PIE

/*
 * 1. Interrupt entry: flag a running loop that the CPU did not.
 *
 * The erratum: the CPU does not always report DIRTY while a loop is running, so a task could be
 * switched out without its loop registers being saved (espressif/esp-idf#19025). ESP-IDF
 * handles this where it restores the registers but not where it saves them. If the state is not
 * DIRTY although a loop counter is not zero, set it to DIRTY; the original routine then saves
 * the registers and marks the task as a hardware-loop user.
 *
 * The interrupted context is already saved, so a0 and t1 are free; the original routine takes
 * no arguments.
 */
    .section .iram1, "ax"
    .global __wrap_rtos_int_enter
    .type __wrap_rtos_int_enter, @function
__wrap_rtos_int_enter:
    csrr    a0, CSR_HWLP_STATE_REG
    beqz    a0, 1f                      /* OFF: no loop can be running */
    csrr    a0, CSR_LOOP0_COUNT
    bnez    a0, 2f
    csrr    a0, CSR_LOOP1_COUNT
    beqz    a0, 1f
2:
    csrwi   CSR_HWLP_STATE_REG, HWLP_DIRTY_STATE
1:
    tail    __real_rtos_int_enter
    .size __wrap_rtos_int_enter, .-__wrap_rtos_int_enter

/*
 * 2. Interrupt exit: do not make the resumed task trap for a PIE unit it already owns.
 *
 * After a context switch the task's first PIE instruction traps. When that instruction sits in
 * a hardware loop, the loop comes back from the trap one iteration short (measured: with PIE
 * left on, 16 of 16 preempted runs were bit-exact; with the lazy trap, most were not). The trap
 * is only needed when another task used PIE on this core in the meantime. If the task being
 * resumed is still the owner of the core's PIE registers they are intact, so PIE is switched
 * back on here and no trap occurs.
 *
 * Limit: with two PIE-using tasks on the same core, the one that lost ownership still takes the
 * trap. This firmware has one such task per core at most (the inference thread on core 1).
 *
 * The original routine returns the new mstatus in a0 and leaves the task's stack in sp; the
 * caller restores every general register from the task's frame afterwards, so t0-t2 and s5 are
 * free (neither the original routine nor the caller uses s5).
 */
    .section .iram1, "ax"
    .global __wrap_rtos_int_exit
    .type __wrap_rtos_int_exit, @function
__wrap_rtos_int_exit:
    mv      s5, ra
    call    __real_rtos_int_exit
    csrr    t0, mhartid
    slli    t1, t0, 2
    la      t2, pxCurrentTCBs
    add     t2, t2, t1
    lw      t2, 0(t2)                   /* t2 = task being resumed on this core */
    li      t1, SOC_CPU_COPROC_NUM * 4
    mul     t0, t0, t1
    la      t1, port_uxCoprocOwner
    add     t1, t1, t0
    lw      t1, PIE_COPROC_IDX * 4(t1)  /* t1 = owner of this core's PIE registers */
    bne     t1, t2, 1f
    li      t0, 1
    csrw    CSR_PIE_STATE_REG, t0
1:
    mv      ra, s5
    ret
    .size __wrap_rtos_int_exit, .-__wrap_rtos_int_exit

#else
#error "hwlp_erratum.S targets the ESP32-P4; remove it and the --wrap linker options elsewhere"
#endif
