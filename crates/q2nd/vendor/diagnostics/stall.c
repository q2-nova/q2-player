/* Optional interrupted-owner sampler. No allocation, formatting, files, or
 * symbol lookup in the signal handler. The observer never sends health pulses.
 * One report per stalled operation; at most six signals in a minute. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
#ifdef Q2_HAVE_UNWIND
#define UNW_LOCAL_ONLY
#include <libunwind.h>
#endif

_Static_assert(ATOMIC_INT_LOCK_FREE == 2, "signal capture needs lock-free words");
enum { OWNER_COUNT = 3, FRAME_COUNT = 48 };
struct sample {
    uint32_t version, role, tid, task, sequence, elapsed_ms, count;
    int32_t status;
    uint32_t generation, reserved;
    uint64_t monotonic_us;
    uint64_t pc[FRAME_COUNT];
};
_Static_assert(sizeof(struct sample) == 432, "fixed wire layout");
struct owner {
    uint32_t tid;
    atomic_uint ready, active, task, entered, request, elapsed;
    atomic_uint active_generation, wall_generation, request_generation;
    atomic_uint wall_active, wall_entered, wall_task;
};
static struct owner owners[OWNER_COUNT];
static _Thread_local struct owner *local;
static _Thread_local uint32_t sequence;
static int output = -1;
static atomic_uint enabled;
static atomic_uint collecting, generation;
static pthread_mutex_t collection_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t collection_changed = PTHREAD_COND_INITIALIZER;
static atomic_uint dropped, notes_remaining;
static int input = -1;
static void (*deliver)(const struct sample *);
static uint32_t millis(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (uint32_t)((uint64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000);
}
static uint64_t boot_us(void) {
    struct timespec now; clock_gettime(CLOCK_BOOTTIME, &now);
    return (uint64_t)now.tv_sec * 1000000 + now.tv_nsec / 1000;
}
/* A nonzero generation identifies one admitted capture context. Read it before
 * work begins; an overlapping disable/re-enable can only make it stale. */
uint32_t q2_stall_generation(void) {
    uint32_t value = atomic_load_explicit(&generation, memory_order_acquire);
    return atomic_load_explicit(&collecting, memory_order_acquire) ? value : 0;
}
static void publish(const struct sample *sample) {
    if (!sample->generation || sample->generation != q2_stall_generation()) return;
    if (send(output, sample, sizeof(*sample), MSG_DONTWAIT | MSG_NOSIGNAL) != sizeof(*sample))
        atomic_fetch_add_explicit(&dropped, 1, memory_order_relaxed);
}
uint32_t q2_stall_dropped(void) { return atomic_load_explicit(&dropped, memory_order_relaxed); }

static void capture(int number, siginfo_t *info, void *context) {
    (void)number; (void)info;
    int saved_errno = errno;
    uint32_t capture_generation = q2_stall_generation();
    struct owner *o = local;
    if (!o || !capture_generation) { errno = saved_errno; return; }
    uint32_t requested = atomic_exchange_explicit(&o->request, 0, memory_order_acquire);
    if (!requested || requested != atomic_load_explicit(&o->active, memory_order_acquire) ||
            capture_generation != atomic_load_explicit(&o->active_generation, memory_order_acquire) ||
            capture_generation != atomic_load_explicit(&o->request_generation, memory_order_acquire)) {
        errno = saved_errno; return;
    }
    struct sample result = {.version = 2, .generation = capture_generation, .role = (uint32_t)(o - owners) + 1,
        .tid = o->tid, .sequence = requested,
        .task = atomic_load_explicit(&o->task, memory_order_relaxed),
        .elapsed_ms = atomic_load_explicit(&o->elapsed, memory_order_relaxed),
        .monotonic_us = boot_us(),
        .status = -1000};
#ifdef Q2_HAVE_UNWIND
    unw_cursor_t cursor;
    result.status = unw_init_local2(&cursor, (unw_context_t *)context, UNW_INIT_SIGNAL_FRAME);
    if (!result.status) {
        do {
            unw_word_t pc;
            if (unw_get_reg(&cursor, UNW_REG_IP, &pc) < 0) { result.status = -1001; break; }
            result.pc[result.count++] = (uint64_t)pc;
            if (result.count == FRAME_COUNT) { result.status = 1; break; }
            result.status = unw_step(&cursor);
        } while (result.status > 0);
    }
#else
    (void)context;
#endif
    publish(&result);
    errno = saved_errno;
}

/* Milliseconds from an operation `elapsed` (< `mark`) ms old to its
 * reporting mark, never under 50 ms. */
static uint32_t until_mark(uint32_t elapsed, uint32_t mark) {
    return mark - elapsed < 50 ? 50 : mark - elapsed;
}

static void *observe(void *unused) {
    (void)unused;
    pthread_setname_np(pthread_self(), "stall-observer");
    uint32_t seen[OWNER_COUNT] = {0}, wall_seen[OWNER_COUNT] = {0}, window = millis(), remaining = 6;
    uint32_t wait_ms = 125;
    while (atomic_load_explicit(&enabled, memory_order_acquire)) {
        pthread_mutex_lock(&collection_lock);
        while (atomic_load_explicit(&enabled, memory_order_acquire) &&
                !atomic_load_explicit(&collecting, memory_order_acquire))
            pthread_cond_wait(&collection_changed, &collection_lock);
        uint32_t capture_generation = q2_stall_generation();
        pthread_mutex_unlock(&collection_lock);
        if (!atomic_load_explicit(&enabled, memory_order_acquire)) break;
        struct timespec wait = {wait_ms / 1000, (long)(wait_ms % 1000) * 1000000};
        nanosleep(&wait, NULL);
        /* Sleep until the next instant a check can report something: the
         * 250 ms mark of an active operation, the 500 ms mark of a wall
         * operation, or 250 ms from now, since an operation that starts
         * after this check reaches 250 ms no sooner. About 4 wakeups/s
         * while nothing is slow, against 8 with the fixed 125 ms period. */
        wait_ms = 250;
        if (!capture_generation || capture_generation != q2_stall_generation()) continue;
        if (deliver) {
            struct sample sample;
            for (unsigned n = 0; n < 16; n++) {
                if (recv(input, &sample, sizeof(sample), MSG_DONTWAIT) != sizeof(sample)) break;
                if (sample.generation == capture_generation) deliver(&sample);
            }
        }
        uint32_t now = millis();
        if ((uint32_t)(now - window) >= 60000) {
            window = now; remaining = 6;
            atomic_store_explicit(&notes_remaining, 6, memory_order_relaxed);
        }
        for (unsigned i = 0; i < OWNER_COUNT; i++) {
            struct owner *o = &owners[i];
            if (!atomic_load_explicit(&o->ready, memory_order_acquire)) continue;
            uint32_t wall = atomic_load_explicit(&o->wall_active, memory_order_acquire);
            uint32_t wall_elapsed = now - atomic_load_explicit(&o->wall_entered, memory_order_relaxed);
            uint32_t wall_task = atomic_load_explicit(&o->wall_task, memory_order_relaxed);
            if (wall && wall != wall_seen[i] && wall_elapsed >= 500 && remaining &&
                    capture_generation == atomic_load_explicit(&o->wall_generation, memory_order_acquire) &&
                    wall == atomic_load_explicit(&o->wall_active, memory_order_acquire)) {
                /* This labels operation latency, including legitimate awaits.
                 * Never sample the thread's unrelated current poll for it. */
                struct sample sample = {.version = 2, .generation = capture_generation, .role = i + 1, .tid = o->tid,
                    .task = wall_task,
                    .sequence = wall, .elapsed_ms = wall_elapsed, .status = -2001,
                    .monotonic_us = boot_us()};
                wall_seen[i] = wall; remaining--;
                publish(&sample);
            }
            if (wall && wall != wall_seen[i] && wall_elapsed < 500 && remaining) {
                uint32_t w = until_mark(wall_elapsed, 500);
                if (w < wait_ms) wait_ms = w;
            }
            uint32_t ticket = atomic_load_explicit(&o->active, memory_order_acquire);
            uint32_t elapsed = now - atomic_load_explicit(&o->entered, memory_order_relaxed);
            if (ticket && ticket != seen[i] && elapsed < 250 && remaining) {
                uint32_t w = until_mark(elapsed, 250);
                if (w < wait_ms) wait_ms = w;
            }
            if (!ticket || ticket == seen[i] || elapsed < 250 || !remaining ||
                    capture_generation != atomic_load_explicit(&o->active_generation, memory_order_acquire)) continue;
            /* The handler rechecks the ticket. A late signal cannot describe
             * an unrelated new operation after the sampled poll has ended. */
            if (atomic_load_explicit(&o->active, memory_order_acquire) != ticket) continue;
            seen[i] = ticket;
            atomic_store_explicit(&o->elapsed, elapsed, memory_order_relaxed);
            atomic_store_explicit(&o->request_generation, capture_generation, memory_order_relaxed);
            atomic_store_explicit(&o->request, ticket, memory_order_release);
            remaining--;
            /* Numeric TIDs remain safe after an owner exits. Never dereference
             * a retired pthread descriptor during shutdown or a thread panic. */
            if (syscall(SYS_tgkill, getpid(), o->tid, SIGUSR2))
                atomic_fetch_add_explicit(&dropped, 1, memory_order_relaxed);
            /* Deliver the captured stack at the former cadence. */
            if (wait_ms > 125) wait_ms = 125;
        }
    }
    return NULL;
}

/* Start once, paused; ordinary owners may register before or after startup.
 * The caller enables only after persisting its context and retains the socket
 * for process lifetime. Refuse to replace an existing signal disposition. */
int q2_stall_start(int fd, int reader, void (*callback)(const struct sample *)) {
    if (output >= 0) return -1;
    struct sigaction old, action = {0};
    if (sigaction(SIGUSR2, NULL, &old) || old.sa_handler != SIG_DFL) return -1;
    action.sa_sigaction = capture;
    action.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR2, &action, NULL)) return -1;
    output = fd;
    input = reader; deliver = callback;
    atomic_store_explicit(&notes_remaining, 6, memory_order_relaxed);
    atomic_store_explicit(&enabled, 1, memory_order_release);
    atomic_store_explicit(&collecting, 0, memory_order_release);
    pthread_t watcher;
    if (pthread_create(&watcher, NULL, observe, NULL)) {
        atomic_store(&enabled, 0); output = -1;
        sigaction(SIGUSR2, &old, NULL); return -1;
    }
    pthread_detach(watcher);
    return 0;
}

/* Registration/prewarming belongs to ordinary thread startup, never a poll or
 * signal handler. Roles are single stable process-lifetime owners: UI, IO, PCM. */
int q2_stall_register(unsigned role) {
    if (role < 1 || role > OWNER_COUNT || local) return -1;
    struct owner *o = &owners[role - 1];
    if (atomic_load_explicit(&o->ready, memory_order_acquire)) return -1;
#ifdef Q2_HAVE_UNWIND
    unw_context_t context; unw_cursor_t cursor;
    unw_getcontext(&context);
    if (unw_init_local(&cursor, &context) < 0) return -1;
    for (unsigned n = 0; n < FRAME_COUNT && unw_step(&cursor) > 0; ++n) {}
#endif
    o->tid = (uint32_t)syscall(SYS_gettid);
    local = o;
    atomic_store_explicit(&o->ready, 1, memory_order_release);
    return 0;
}

/* Pause without a periodic wake after opt-out. Registered owner identities
 * remain available for a later scoped test; interrupted work cannot cross it. */
uint32_t q2_stall_collecting(int value) {
    pthread_mutex_lock(&collection_lock);
    atomic_store_explicit(&collecting, 0, memory_order_release);
    uint32_t current = atomic_load_explicit(&generation, memory_order_relaxed);
    /* Never reuse a generation: exhaustion fails closed for this process. */
    uint32_t next = current == UINT32_MAX ? 0 : current + 1;
    if (next) atomic_store_explicit(&generation, next, memory_order_release);
    for (unsigned i = 0; i < OWNER_COUNT; i++) {
        atomic_store(&owners[i].active, 0);
        atomic_store(&owners[i].wall_active, 0);
        atomic_store(&owners[i].request, 0);
    }
    if (value && next) atomic_store_explicit(&collecting, 1, memory_order_release);
    pthread_cond_signal(&collection_changed);
    pthread_mutex_unlock(&collection_lock);
    return next;
}

uint32_t q2_stall_enter(uint32_t task) {
    uint32_t capture_generation = q2_stall_generation();
    if (!local || !capture_generation) return 0;
    /* Nested instrumentation must leave the outer operation's ticket intact. */
    if (atomic_load_explicit(&local->active, memory_order_relaxed)) return 0;
    if (!++sequence) ++sequence;
    atomic_store_explicit(&local->active_generation, capture_generation, memory_order_relaxed);
    atomic_store_explicit(&local->task, task, memory_order_relaxed);
    atomic_store_explicit(&local->entered, millis(), memory_order_relaxed);
    atomic_store_explicit(&local->active, sequence, memory_order_release);
    if (capture_generation != q2_stall_generation()) {
        uint32_t ticket = sequence;
        atomic_compare_exchange_strong_explicit(&local->active, &ticket, 0,
            memory_order_release, memory_order_relaxed);
        return 0;
    }
    return sequence;
}
void q2_stall_leave(uint32_t ticket) {
    if (local && ticket) atomic_compare_exchange_strong_explicit(&local->active, &ticket, 0,
        memory_order_release, memory_order_relaxed);
}

/* Completed wall-latency observations have no execution stack. In particular,
 * time spent awaiting I/O must not be labelled an executor-blocking poll. */
void q2_stall_note(uint32_t role, uint32_t task, uint32_t elapsed_ms, uint32_t capture_generation) {
    if (!capture_generation || capture_generation != q2_stall_generation() || elapsed_ms < 250) return;
    unsigned budget = atomic_load_explicit(&notes_remaining, memory_order_relaxed);
    while (budget && !atomic_compare_exchange_weak_explicit(&notes_remaining, &budget, budget - 1,
            memory_order_relaxed, memory_order_relaxed)) {}
    if (!budget) { atomic_fetch_add_explicit(&dropped, 1, memory_order_relaxed); return; }
    struct sample sample = {.version = 2, .generation = capture_generation, .role = role, .task = task,
        .tid = (uint32_t)syscall(SYS_gettid), .elapsed_ms = elapsed_ms, .status = -2000,
        .monotonic_us = boot_us()};
    publish(&sample);
}
uint32_t q2_stall_wall_enter(uint32_t task) {
    uint32_t capture_generation = q2_stall_generation();
    if (!local || !capture_generation || atomic_load_explicit(&local->wall_active, memory_order_relaxed)) return 0;
    if (!++sequence) ++sequence;
    atomic_store_explicit(&local->wall_generation, capture_generation, memory_order_relaxed);
    atomic_store_explicit(&local->wall_task, task, memory_order_relaxed);
    atomic_store_explicit(&local->wall_entered, millis(), memory_order_relaxed);
    atomic_store_explicit(&local->wall_active, sequence, memory_order_release);
    if (capture_generation != q2_stall_generation()) {
        uint32_t ticket = sequence;
        atomic_compare_exchange_strong_explicit(&local->wall_active, &ticket, 0,
            memory_order_release, memory_order_relaxed);
        return 0;
    }
    return sequence;
}
void q2_stall_wall_leave(uint32_t ticket) {
    if (local && ticket) atomic_compare_exchange_strong_explicit(&local->wall_active, &ticket, 0,
        memory_order_release, memory_order_relaxed);
}
