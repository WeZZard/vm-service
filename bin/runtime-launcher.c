/* Service-owned guest launcher. Build/stage during separately reviewed provisioning.
 * No sudo, shell, password, user-controlled argv/environment, or guest JSON proof.
 * Host must verify this binary and the complete immutable dependency manifest over
 * the lease channel before use, plus independently attest the process credentials.
 * Does NOT create/lock accounts: never use until clone provisioning is accepted.
 */
#include <sys/types.h>
#include <sys/stat.h>
#include <unistd.h>
#include <grp.h>
#include <pwd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <limits.h>

#define ROOT "/var/tmp/recorder-rpc"
static void fail(const char *s) { fprintf(stderr, "runtime-launcher: %s\n", s); exit(73); }
static void immutable(const char *p, int dir) {
    struct stat s;
    if (lstat(p, &s) || s.st_uid != 0 || (s.st_mode & 022) ||
        (dir ? !S_ISDIR(s.st_mode) : !S_ISREG(s.st_mode)) ||
        (!dir && s.st_nlink != 1)) fail("unsafe immutable path");
}
int main(int argc, char **argv) {
    (void)argv;
    if (argc != 1 || getuid() != 0 || geteuid() != 0) fail("requires service root launch, no arguments");
    struct passwd *pw = getpwnam("vmruntime");
    if (!pw || pw->pw_uid < 1000 || pw->pw_gid < 1000 || pw->pw_uid == 65534 ||
        strcmp(pw->pw_dir, ROOT "/output")) fail("runtime account not isolated");
    uid_t uid = pw->pw_uid; gid_t gid = pw->pw_gid;
    struct group *gr = getgrgid(gid);
    if (!gr || strcmp(gr->gr_name, "vmruntime")) fail("wrong primary group");
    /* Check every group, including supplementary privilege grants. */
    setgrent();
    while ((gr = getgrent())) {
        for (char **member = gr->gr_mem; member && *member; ++member)
            if (!strcmp(*member, "vmruntime") && gr->gr_gid != gid)
                fail("runtime has supplementary membership");
    }
    endgrent();
    /* /var aliases /private/var on macOS; validate the canonical ancestors. */
    char resolved[PATH_MAX];
    if (!realpath(ROOT, resolved)) fail("missing staging root");
    if (strcmp(resolved, ROOT) && strcmp(resolved, "/private" ROOT)) fail("aliased staging root");
    immutable(resolved, 1);
    immutable(ROOT "/code", 1);
    immutable(ROOT "/code/node", 0);
    immutable(ROOT "/code/adapter.mjs", 0);
    immutable(ROOT "/code/runtime.json", 0);
    struct stat out;
    if (lstat(ROOT "/output", &out) || !S_ISDIR(out.st_mode) || out.st_uid != uid ||
        out.st_gid != gid || (out.st_mode & 077)) fail("unsafe output directory");
    if (setgroups(1, &gid) || setgid(gid) || setuid(uid)) fail("credential drop failed");
    gid_t groups[2]; int n = getgroups(2, groups);
    if (getuid() != uid || geteuid() != uid || getgid() != gid || getegid() != gid ||
        n != 1 || groups[0] != gid) fail("kernel credential verification failed");
    errno = 0;
    if (setuid(0) == 0 || errno != EPERM) fail("root identity recoverable");
    if (chdir(ROOT "/output")) fail("cannot enter output directory");
    umask(077);
    /* No inherited descriptors except the service SSH stdin/stdout/stderr. */
    long maxfd = sysconf(_SC_OPEN_MAX);
    if (maxfd < 0) fail("cannot bound descriptor cleanup");
    for (int fd = 3; fd < maxfd; ++fd) close(fd);
    char *const args[] = {ROOT "/code/node", ROOT "/code/adapter.mjs", "--config", ROOT "/code/runtime.json", NULL};
    char *const env[] = {"HOME=" ROOT "/output", "TMPDIR=" ROOT "/output", "PATH=/usr/bin:/bin", "LANG=C", NULL};
    execve(args[0], args, env);
    fail("exec failed");
}
