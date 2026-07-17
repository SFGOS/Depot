#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/xattr.h>
#include <unistd.h>

#define DEPOT_UID_XATTR "user.depot.fakeroot.uid"
#define DEPOT_GID_XATTR "user.depot.fakeroot.gid"

static uid_t mapped_uid(uid_t uid) {
    return uid == (uid_t)-1 ? uid : 0;
}

static gid_t mapped_gid(gid_t gid) {
    return gid == (gid_t)-1 ? gid : 0;
}

static int missing_xattr_error(int error) {
    return error == ENODATA || error == ENOTSUP;
}

static int update_id_path(const char *path, const char *name, unsigned int id,
                          int nofollow) {
    char value[16];
    int length = snprintf(value, sizeof(value), "%u", id);
    if (length < 0 || (size_t)length >= sizeof(value)) {
        errno = EOVERFLOW;
        return -1;
    }

    if (id == 0) {
        int result = nofollow != 0 ? lremovexattr(path, name)
                                   : removexattr(path, name);
        if (result == 0 || missing_xattr_error(errno) ||
            (nofollow != 0 && errno == EPERM)) {
            return 0;
        }
        return -1;
    }

    if (nofollow != 0) {
        return lsetxattr(path, name, value, (size_t)length, 0);
    }
    return setxattr(path, name, value, (size_t)length, 0);
}

static int update_id_fd(int fd, const char *name, unsigned int id) {
    char value[16];
    int length = snprintf(value, sizeof(value), "%u", id);
    if (length < 0 || (size_t)length >= sizeof(value)) {
        errno = EOVERFLOW;
        return -1;
    }

    if (id == 0) {
        int result = fremovexattr(fd, name);
        if (result == 0 || missing_xattr_error(errno)) {
            return 0;
        }
        return -1;
    }
    return fsetxattr(fd, name, value, (size_t)length, 0);
}

static int record_path(const char *path, uid_t uid, gid_t gid, int nofollow) {
    if (uid != (uid_t)-1 &&
        update_id_path(path, DEPOT_UID_XATTR, (unsigned int)uid, nofollow) !=
            0) {
        return -1;
    }
    if (gid != (gid_t)-1 &&
        update_id_path(path, DEPOT_GID_XATTR, (unsigned int)gid, nofollow) !=
            0) {
        return -1;
    }
    return 0;
}

static int record_fd(int fd, uid_t uid, gid_t gid) {
    if (uid != (uid_t)-1 &&
        update_id_fd(fd, DEPOT_UID_XATTR, (unsigned int)uid) != 0) {
        return -1;
    }
    if (gid != (gid_t)-1 &&
        update_id_fd(fd, DEPOT_GID_XATTR, (unsigned int)gid) != 0) {
        return -1;
    }
    return 0;
}

int chown(const char *path, uid_t uid, gid_t gid) {
    if (syscall(SYS_chown, path, mapped_uid(uid), mapped_gid(gid)) != 0) {
        return -1;
    }
    return record_path(path, uid, gid, 0);
}

int lchown(const char *path, uid_t uid, gid_t gid) {
    if (syscall(SYS_lchown, path, mapped_uid(uid), mapped_gid(gid)) != 0) {
        return -1;
    }
    return record_path(path, uid, gid, 1);
}

int fchown(int fd, uid_t uid, gid_t gid) {
    if (syscall(SYS_fchown, fd, mapped_uid(uid), mapped_gid(gid)) != 0) {
        return -1;
    }
    return record_fd(fd, uid, gid);
}

int fchownat(int dirfd, const char *path, uid_t uid, gid_t gid, int flags) {
    int fd;
    int open_flags = O_RDONLY | O_CLOEXEC | O_NONBLOCK;

    if (syscall(SYS_fchownat, dirfd, path, mapped_uid(uid), mapped_gid(gid),
                flags) != 0) {
        return -1;
    }

    if ((flags & AT_SYMLINK_NOFOLLOW) != 0) {
        if (dirfd == AT_FDCWD || path[0] == '/') {
            return record_path(path, uid, gid, 1);
        }
        errno = ENOTSUP;
        return -1;
    }

    fd = openat(dirfd, path, open_flags);
    if (fd < 0) {
        return -1;
    }
    if (record_fd(fd, uid, gid) != 0) {
        int saved_errno = errno;
        close(fd);
        errno = saved_errno;
        return -1;
    }
    return close(fd);
}
