// Internal, framed operation ABI used only between our FSKit adapter and Rust
// engine. Layout follows the vendored fuser ABI 7.31 on Darwin; no macFUSE API.
#pragma once
#include <stdint.h>
typedef struct { uint32_t len,opcode; uint64_t unique,node; uint32_t uid,gid,pid,padding; } GORequest;
typedef struct { uint32_t len; int32_t error; uint64_t unique; } GOResponse;
typedef struct {
 uint64_t ino,size,blocks; int64_t atime,mtime,ctime; uint64_t birth;
 uint32_t atimens,mtimens,ctimens,birthns,mode,nlink,uid,gid,rdev,flags,blksize,padding;
} GOAttr;
typedef struct { uint64_t node,generation,entryTTL,attrTTL; uint32_t entryNS,attrNS; GOAttr attr; } GOEntry;
typedef struct { uint64_t ttl; uint32_t ns,padding; GOAttr attr; } GOAttributes;
typedef struct { uint32_t flags,unused; } GOOpen;
typedef struct { uint64_t handle; uint32_t flags,padding; } GOHandle;
typedef struct { uint64_t handle; int64_t offset; uint32_t size,options; uint64_t owner; uint32_t flags,padding; } GOReadWrite;
typedef struct { uint64_t handle; uint32_t flags,options; uint64_t owner; } GORelease;
typedef struct {
 uint32_t valid,padding; uint64_t handle,size,owner; int64_t atime,mtime,ctime;
 uint32_t atimens,mtimens,ctimens,mode,unused,uid,gid,unused2;
 uint64_t backup,change,birth; uint32_t backupns,changens,birthns,flags;
} GOSetAttr;
typedef struct { uint64_t ino,next; uint32_t length,type; } GODirent;
typedef struct { uint64_t blocks,bfree,bavail,files,ffree; uint32_t bsize,namelen,frsize,padding,spare[6]; } GOStat;
_Static_assert(sizeof(GORequest)==40 && sizeof(GOAttr)==104 && sizeof(GOSetAttr)==128,"internal ABI drift");
