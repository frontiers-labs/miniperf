// Deny only perf_event_open, then run an otherwise unchanged benchmark process.
#include <errno.h>
#include <stddef.h>
#include <stdio.h>
#include <unistd.h>

#ifdef __linux__
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <sys/prctl.h>
#include <sys/syscall.h>

int main(int argc, char **argv) {
  if (argc < 2) {
    fprintf(stderr, "usage: deny_perf PROGRAM [ARG ...]\n");
    return 2;
  }
  struct sock_filter instructions[] = {
      BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
      BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_perf_event_open, 0, 1),
      BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
      BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
  };
  struct sock_fprog filter = {
      .len = sizeof(instructions) / sizeof(instructions[0]),
      .filter = instructions,
  };
  if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 ||
      prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &filter) != 0) {
    perror("deny_perf: seccomp");
    return 1;
  }
  errno = 0;
  if (syscall(SYS_perf_event_open, NULL, 0, -1, -1, 0) != -1 || errno != EPERM) {
    fprintf(stderr, "deny_perf: perf_event_open was not denied with EPERM\n");
    return 1;
  }
  execvp(argv[1], argv + 1);
  perror("deny_perf: execvp");
  return 1;
}
#else
int main(void) {
  fprintf(stderr, "deny_perf requires Linux seccomp\n");
  return 1;
}
#endif
