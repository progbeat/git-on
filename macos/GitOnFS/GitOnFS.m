#import "GitOnFS.h"
#import "Wire.h"
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/stat.h>
#include <sys/mount.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <dirent.h>

static NSError *failure(int code) { return [NSError errorWithDomain:NSPOSIXErrorDomain code:code userInfo:nil]; }
static NSData *bytes(const void *p, size_t n) { return [NSData dataWithBytes:p length:n]; }
static NSData *nameBytes(FSFileName *name) {
    NSMutableData *result=[name.data mutableCopy]; uint8_t zero=0; [result appendBytes:&zero length:1]; return result;
}
static BOOL transfer(int fd, void *buffer, size_t length, BOOL writing) {
    uint8_t *p=buffer;
    while(length) { ssize_t n=writing?write(fd,p,length):read(fd,p,length); if(n<0 && errno==EINTR)continue; if(n<=0)return NO; p+=n;length-=n; }
    return YES;
}
@interface GOItem : FSItem
@property uint64_t inode, references, handle;
@property uint32_t flags;
@property FSItemType kind;
@end
@implementation GOItem
@end

@interface GOVolume : FSVolume <FSVolumeOperations,FSVolumeOpenCloseOperations,FSVolumeReadWriteOperations,FSVolumeAccessCheckOperations>
@property int socketFD;
@property uint64_t sequence, namespaceVersion;
@property NSURL *resourceURL;
@property BOOL readOnly;
@property NSMutableDictionary<NSNumber *,GOItem *> *items;
- (instancetype)initWithURL:(NSURL *)url error:(NSError **)error;
@end
@implementation GOVolume
- (instancetype)initWithURL:(NSURL *)url error:(NSError **)error {
    self=[super initWithVolumeID:[FSVolumeIdentifier new] volumeName:[[FSFileName alloc] initWithString:@"Git-on"]];
    if(!self)return nil;
    _socketFD=-1; _resourceURL=url; _namespaceVersion=1; _items=[NSMutableDictionary new];
    [url startAccessingSecurityScopedResource];
    struct stat st;
    if(lstat(url.fileSystemRepresentation,&st) || !S_ISDIR(st.st_mode) || st.st_uid!=geteuid() || (st.st_mode&077)!=0) { *error=failure(EACCES); return nil; }
    NSURL *endpoint=[url URLByAppendingPathComponent:@"bridge.sock"];
    if(lstat(endpoint.fileSystemRepresentation,&st) || !S_ISSOCK(st.st_mode) || st.st_uid!=geteuid() || (st.st_mode&077)!=0) { *error=failure(EACCES);return nil; }
    struct sockaddr_un address={.sun_family=AF_UNIX};
    if(strlen(endpoint.fileSystemRepresentation)>=sizeof(address.sun_path)){*error=failure(ENAMETOOLONG);return nil;}
    strlcpy(address.sun_path,endpoint.fileSystemRepresentation,sizeof(address.sun_path));
    _socketFD=socket(AF_UNIX,SOCK_STREAM,0);
    if(_socketFD<0 || connect(_socketFD,(struct sockaddr *)&address,sizeof(address))) { *error=failure(errno);return nil; }
    uid_t uid;gid_t gid;
    if(getpeereid(_socketFD,&uid,&gid) || uid!=geteuid()){*error=failure(EACCES);return nil;}
    int one=1;setsockopt(_socketFD,SOL_SOCKET,SO_NOSIGPIPE,&one,sizeof(one));
    struct timeval timeout={.tv_sec=60};setsockopt(_socketFD,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout));setsockopt(_socketFD,SOL_SOCKET,SO_SNDTIMEO,&timeout,sizeof(timeout));
    uint32_t init[]={7,31,0,0};
    if(![self request:26 node:0 body:bytes(init,sizeof(init)) error:error])return nil;
    // Policy is supplied by the owner CLI on this authenticated per-run channel.
    // Its immutable RO marker is metadata, not executable configuration.
    _readOnly=[[NSFileManager defaultManager] fileExistsAtPath:[[url URLByAppendingPathComponent:@"readonly"] path]];
    return self;
}
- (void)dealloc { if(_socketFD>=0)close(_socketFD);[_resourceURL stopAccessingSecurityScopedResource]; }
- (NSData *)request:(uint32_t)opcode node:(uint64_t)node body:(NSData *)body error:(NSError **)error {
    @synchronized(self) {
        GORequest header={.len=(uint32_t)(sizeof(header)+body.length),.opcode=opcode,.unique=++_sequence,.node=node,.uid=geteuid(),.gid=getegid(),.pid=0};
        uint32_t length=header.len;
        if(length>16*1024*1024 || !transfer(_socketFD,&length,4,YES) || !transfer(_socketFD,&header,sizeof(header),YES) || !transfer(_socketFD,(void *)body.bytes,body.length,YES)) { *error=failure(errno?:EIO);return nil; }
        if(!transfer(_socketFD,&length,4,NO) || length<sizeof(GOResponse) || length>16*1024*1024){*error=failure(EIO);return nil;}
        NSMutableData *response=[NSMutableData dataWithLength:length];
        if(!transfer(_socketFD,response.mutableBytes,length,NO)){*error=failure(errno?:EIO);return nil;}
        GOResponse reply;memcpy(&reply,response.bytes,sizeof(reply));
        if(reply.len!=length || reply.unique!=header.unique){*error=failure(EPROTO);return nil;}
        if(reply.error){*error=failure(-reply.error);return nil;}
        return [response subdataWithRange:NSMakeRange(sizeof(reply),length-sizeof(reply))];
    }
}
- (GOItem *)item:(uint64_t)inode kind:(FSItemType)kind retainLookup:(BOOL)retainLookup {
    GOItem *item=_items[@(inode)];if(!item){item=[GOItem new];item.inode=inode;item.kind=kind;_items[@(inode)]=item;}
    if(retainLookup)item.references++; return item;
}
static FSItemType itemType(uint32_t mode) { switch(mode&S_IFMT){case S_IFDIR:return FSItemTypeDirectory;case S_IFLNK:return FSItemTypeSymlink;default:return FSItemTypeFile;} }
- (GOItem *)entry:(NSData *)data error:(NSError **)error {
    if(data.length<sizeof(GOEntry)){*error=failure(EPROTO);return nil;}GOEntry entry;memcpy(&entry,data.bytes,sizeof(entry));return [self item:entry.node kind:itemType(entry.attr.mode) retainLookup:YES];
}
- (FSItemAttributes *)attributes:(GOAttr)a {
    FSItemAttributes *result=[FSItemAttributes new]; result.type=itemType(a.mode);result.mode=a.mode&0777;result.uid=a.uid;result.gid=a.gid;result.linkCount=a.nlink;result.size=a.size;result.allocSize=a.blocks*512;result.fileID=(FSItemID)(a.ino+1);
    result.accessTime=(struct timespec){a.atime,a.atimens};result.modifyTime=(struct timespec){a.mtime,a.mtimens};result.changeTime=(struct timespec){a.ctime,a.ctimens};result.birthTime=(struct timespec){a.birth,a.birthns};result.flags=0;return result;
}
- (FSItemAttributes *)attributesOf:(GOItem *)item error:(NSError **)error {
    uint64_t input[]={0,0};NSData *data=[self request:3 node:item.inode body:bytes(input,sizeof(input)) error:error];if(!data)return nil;
    if(data.length<sizeof(GOAttributes)){*error=failure(EPROTO);return nil;}GOAttributes attributes;memcpy(&attributes,data.bytes,sizeof(attributes));return [self attributes:attributes.attr];
}
- (NSInteger)maximumLinkCount{return UINT32_MAX;}
- (NSInteger)maximumNameLength{return 255;}
- (BOOL)restrictsOwnershipChanges{return YES;}
- (BOOL)truncatesLongNames{return NO;}
- (NSInteger)maximumFileSizeInBits{return 64;}
- (NSInteger)maximumXattrSize{return 0;}
- (FSVolumeSupportedCapabilities *)supportedVolumeCapabilities {
    FSVolumeSupportedCapabilities *c=[FSVolumeSupportedCapabilities new];c.supportsSymbolicLinks=YES;c.supportsHardLinks=YES;c.caseFormat=FSVolumeCaseFormatSensitive;return c;
}
- (FSStatFSResult *)volumeStatistics {
    @synchronized(self){NSError *error=nil;NSData *data=[self request:17 node:1 body:[NSData data] error:&error];FSStatFSResult *s=[[FSStatFSResult alloc] initWithFileSystemTypeName:@"giton"];
    if(data.length>=sizeof(GOStat)){GOStat a;memcpy(&a,data.bytes,sizeof(a));s.blockSize=a.bsize;s.ioSize=a.bsize;s.totalBlocks=a.blocks;s.freeBlocks=a.bfree;s.availableBlocks=a.bavail;s.totalFiles=a.files;s.freeFiles=a.ffree;}return s;}
}
- (FSMountOptions)requestedMountOptions{return _readOnly?FSMountOptionsReadOnly:0;}
- (void)mountWithOptions:(FSTaskOptions *)options replyHandler:(void(^)(NSError *))reply{reply(nil);}
- (void)unmountWithReplyHandler:(void(^)(void))reply { shutdown(_socketFD,SHUT_RDWR);reply(); }
- (void)activateWithOptions:(FSTaskOptions *)options replyHandler:(void(^)(FSItem *,NSError *))reply { @synchronized(self){reply([self item:1 kind:FSItemTypeDirectory retainLookup:NO],nil);} }
- (void)deactivateWithOptions:(FSDeactivateOptions)options replyHandler:(void(^)(NSError *))reply {reply(nil);}
- (void)getAttributes:(FSItemGetAttributesRequest *)request ofItem:(FSItem *)item replyHandler:(void(^)(FSItemAttributes *,NSError *))reply { @synchronized(self){NSError *error=nil;FSItemAttributes *a=[self attributesOf:(GOItem *)item error:&error];reply(a,error);} }
- (void)setAttributes:(FSItemSetAttributesRequest *)attributes onItem:(FSItem *)item replyHandler:(void(^)(FSItemAttributes *,NSError *))reply {
    @synchronized(self){GOSetAttr set={0};
    if([attributes isValid:FSItemAttributeMode]){set.valid|=1;set.mode=attributes.mode;}
    if([attributes isValid:FSItemAttributeUID]){set.valid|=2;set.uid=attributes.uid;}
    if([attributes isValid:FSItemAttributeGID]){set.valid|=4;set.gid=attributes.gid;}
    if([attributes isValid:FSItemAttributeSize]){set.valid|=8;set.size=attributes.size;}
    if([attributes isValid:FSItemAttributeAccessTime]){set.valid|=16;set.atime=attributes.accessTime.tv_sec;set.atimens=(uint32_t)attributes.accessTime.tv_nsec;}
    if([attributes isValid:FSItemAttributeModifyTime]){set.valid|=32;set.mtime=attributes.modifyTime.tv_sec;set.mtimens=(uint32_t)attributes.modifyTime.tv_nsec;}
    if([attributes isValid:FSItemAttributeBirthTime]){set.valid|=1<<28;set.birth=attributes.birthTime.tv_sec;set.birthns=(uint32_t)attributes.birthTime.tv_nsec;}
    if([attributes isValid:FSItemAttributeFlags] && attributes.flags){reply(nil,failure(ENOTSUP));return;}
    NSError *error=nil;NSData *data=[self request:4 node:((GOItem *)item).inode body:bytes(&set,sizeof(set)) error:&error];
    if(!data){reply(nil,error);return;}if(data.length<sizeof(GOAttributes)){reply(nil,failure(EPROTO));return;}
    GOAttributes out;memcpy(&out,data.bytes,sizeof(out));
    FSItemAttribute supported=FSItemAttributeMode|FSItemAttributeUID|FSItemAttributeGID|FSItemAttributeSize|FSItemAttributeAccessTime|FSItemAttributeModifyTime|FSItemAttributeBirthTime|FSItemAttributeFlags;
    for(unsigned bit=0;bit<18;bit++){FSItemAttribute flag=(FSItemAttribute)(1UL<<bit);if((supported&flag) && [attributes isValid:flag])attributes.consumedAttributes|=flag;}
    reply([self attributes:out.attr],nil);}
}
- (void)lookupItemNamed:(FSFileName *)name inDirectory:(FSItem *)directory replyHandler:(void(^)(FSItem *,FSFileName *,NSError *))reply {
    @synchronized(self){NSError *error=nil;NSData *data=[self request:1 node:((GOItem *)directory).inode body:nameBytes(name) error:&error];GOItem *item=data?[self entry:data error:&error]:nil;reply(item,item?name:nil,error);}
}
- (void)reclaimItem:(FSItem *)item replyHandler:(void(^)(NSError *))reply {
    @synchronized(self){GOItem *i=(GOItem *)item;
    // FORGET has no reply; send its complete frame without awaiting a response.
    if(i.references){GORequest h={.len=48,.opcode=2,.unique=++_sequence,.node=i.inode,.uid=geteuid(),.gid=getegid()};uint32_t n=h.len;transfer(_socketFD,&n,4,YES);transfer(_socketFD,&h,sizeof(h),YES);uint64_t references=i.references;transfer(_socketFD,&references,8,YES);}
    [_items removeObjectForKey:@(i.inode)];reply(nil);}
}
- (void)readSymbolicLink:(FSItem *)item replyHandler:(void(^)(FSFileName *,NSError *))reply {
    @synchronized(self){NSError *error=nil;NSData *data=[self request:5 node:((GOItem *)item).inode body:[NSData data] error:&error];reply(data?[[FSFileName alloc] initWithData:data]:nil,error);}
}
- (void)createItemNamed:(FSFileName *)name type:(FSItemType)type inDirectory:(FSItem *)directory attributes:(FSItemSetAttributesRequest *)attributes replyHandler:(void(^)(FSItem *,FSFileName *,NSError *))reply {
    @synchronized(self){if(type!=FSItemTypeFile && type!=FSItemTypeDirectory){reply(nil,nil,failure(ENOTSUP));return;}
    uint32_t input[]={attributes.mode|(type==FSItemTypeDirectory?S_IFDIR:S_IFREG),0,0,0};NSMutableData *body=[bytes(input,type==FSItemTypeDirectory?8:16) mutableCopy];[body appendData:nameBytes(name)];
    NSError *error=nil;NSData *data=[self request:type==FSItemTypeDirectory?9:8 node:((GOItem *)directory).inode body:body error:&error];GOItem *item=data?[self entry:data error:&error]:nil;if(item)_namespaceVersion++;reply(item,item?name:nil,error);}
}
- (void)createSymbolicLinkNamed:(FSFileName *)name inDirectory:(FSItem *)directory attributes:(FSItemSetAttributesRequest *)attributes linkContents:(FSFileName *)contents replyHandler:(void(^)(FSItem *,FSFileName *,NSError *))reply {
    @synchronized(self){NSMutableData *body=[nameBytes(name) mutableCopy];[body appendData:nameBytes(contents)];NSError *error=nil;NSData *data=[self request:6 node:((GOItem *)directory).inode body:body error:&error];GOItem *item=data?[self entry:data error:&error]:nil;if(item)_namespaceVersion++;reply(item,item?name:nil,error);}
}
- (void)createLinkToItem:(FSItem *)item named:(FSFileName *)name inDirectory:(FSItem *)directory replyHandler:(void(^)(FSFileName *,NSError *))reply {
    @synchronized(self){uint64_t inode=((GOItem *)item).inode;NSMutableData *body=[bytes(&inode,8) mutableCopy];[body appendData:nameBytes(name)];NSError *error=nil;NSData *data=[self request:13 node:((GOItem *)directory).inode body:body error:&error];if(data){[self entry:data error:&error];_namespaceVersion++;}reply(error?nil:name,error);}
}
- (void)removeItem:(FSItem *)item named:(FSFileName *)name fromDirectory:(FSItem *)directory replyHandler:(void(^)(NSError *))reply {
    @synchronized(self){NSError *error=nil;NSData *data=[self request:((GOItem *)item).kind==FSItemTypeDirectory?11:10 node:((GOItem *)directory).inode body:nameBytes(name) error:&error];if(data)_namespaceVersion++;reply(error);}
}
- (void)renameItem:(FSItem *)item inDirectory:(FSItem *)source named:(FSFileName *)name toNewName:(FSFileName *)newName inDirectory:(FSItem *)destination overItem:(FSItem *)over replyHandler:(void(^)(FSFileName *,NSError *))reply {
    @synchronized(self){uint64_t dir=((GOItem *)destination).inode;NSMutableData *body=[bytes(&dir,8) mutableCopy];[body appendData:nameBytes(name)];[body appendData:nameBytes(newName)];NSError *error=nil;NSData *data=[self request:12 node:((GOItem *)source).inode body:body error:&error];if(data)_namespaceVersion++;reply(error?nil:newName,error);}
}
- (void)enumerateDirectory:(FSItem *)directory startingAtCookie:(FSDirectoryCookie)cookie verifier:(FSDirectoryVerifier)verifier providingAttributes:(FSItemGetAttributesRequest *)attributes usingPacker:(FSDirectoryEntryPacker *)packer replyHandler:(void(^)(FSDirectoryVerifier,NSError *))reply {
    @synchronized(self){if(verifier && verifier!=_namespaceVersion){reply(0,failure(EINVAL));return;}
    NSError *error=nil;GOOpen open={0};NSData *data=[self request:27 node:((GOItem *)directory).inode body:bytes(&open,sizeof(open)) error:&error];if(data.length<sizeof(GOHandle)){reply(0,error?:failure(EPROTO));return;}
    GOHandle handle;memcpy(&handle,data.bytes,sizeof(handle));GOReadWrite read={.handle=handle.handle,.offset=(int64_t)cookie,.size=32768};data=[self request:28 node:((GOItem *)directory).inode body:bytes(&read,sizeof(read)) error:&error];
    NSUInteger offset=0;
    while(data && offset+sizeof(GODirent)<=data.length){GODirent entry;memcpy(&entry,(const uint8_t *)data.bytes+offset,sizeof(entry));NSUInteger size=(sizeof(entry)+entry.length+7)&~7UL;if(entry.length>255 || size>data.length-offset){error=failure(EPROTO);break;}
    NSData *name=[data subdataWithRange:NSMakeRange(offset+sizeof(entry),entry.length)];offset+=size;
    if(attributes && ([name isEqual:bytes(".",1)] || [name isEqual:bytes("..",2)]))continue;
    FSItemAttributes *a=nil;if(attributes){GOItem *i=[self item:entry.ino kind:FSItemTypeUnknown retainLookup:NO];a=[self attributesOf:i error:&error];if(!a)break;}
    FSItemType kind=entry.type==DT_DIR?FSItemTypeDirectory:entry.type==DT_LNK?FSItemTypeSymlink:FSItemTypeFile;
    if(![packer packEntryWithName:[[FSFileName alloc] initWithData:name] itemType:kind itemID:(FSItemID)(entry.ino+1) nextCookie:entry.next attributes:a])break;}
    GORelease close={.handle=handle.handle};NSError *closeError=nil;[self request:29 node:((GOItem *)directory).inode body:bytes(&close,sizeof(close)) error:&closeError];reply(_namespaceVersion,error?:closeError);}
}
- (BOOL)ensureOpen:(GOItem *)item modes:(FSVolumeOpenModes)modes error:(NSError **)error {
    uint32_t flags=(modes&FSVolumeOpenModesWrite)?O_RDWR:O_RDONLY;
    if(item.handle && (item.flags==O_RDWR || flags==O_RDONLY))return YES;
    GOOpen open={.flags=flags};NSData *data=[self request:14 node:item.inode body:bytes(&open,sizeof(open)) error:error];if(data.length<sizeof(GOHandle)){if(!*error)*error=failure(EPROTO);return NO;}
    GOHandle handle;memcpy(&handle,data.bytes,sizeof(handle));if(item.handle){GORelease release={.handle=item.handle,.flags=item.flags};[self request:18 node:item.inode body:bytes(&release,sizeof(release)) error:error];if(*error)return NO;}
    item.handle=handle.handle;item.flags=flags;return YES;
}
- (void)openItem:(FSItem *)item withModes:(FSVolumeOpenModes)modes replyHandler:(void(^)(NSError *))reply {@synchronized(self){NSError *error=nil;[self ensureOpen:(GOItem *)item modes:modes error:&error];reply(error);} }
- (void)closeItem:(FSItem *)item keepingModes:(FSVolumeOpenModes)modes replyHandler:(void(^)(NSError *))reply {@synchronized(self){GOItem *i=(GOItem *)item;NSError *error=nil;if(!modes && i.handle){GORelease release={.handle=i.handle,.flags=i.flags};[self request:18 node:i.inode body:bytes(&release,sizeof(release)) error:&error];i.handle=0;}reply(error);} }
- (void)readFromFile:(FSItem *)item offset:(off_t)offset length:(size_t)length intoBuffer:(FSMutableFileDataBuffer *)buffer replyHandler:(void(^)(size_t,NSError *))reply {
    @synchronized(self){GOItem *i=(GOItem *)item;NSError *error=nil;size_t total=0;if([self ensureOpen:i modes:FSVolumeOpenModesRead error:&error])while(total<length){GOReadWrite input={.handle=i.handle,.offset=offset+(off_t)total,.size=(uint32_t)MIN(length-total,512*1024)};NSData *data=[self request:15 node:i.inode body:bytes(&input,sizeof(input)) error:&error];if(!data)break;if(data.length>input.size || total+data.length>buffer.length){error=failure(EPROTO);break;}memcpy((uint8_t *)buffer.mutableBytes+total,data.bytes,data.length);total+=data.length;if(data.length<input.size)break;}reply(total,error);}
}
- (void)writeContents:(NSData *)contents toFile:(FSItem *)item atOffset:(off_t)offset replyHandler:(void(^)(size_t,NSError *))reply {
    @synchronized(self){GOItem *i=(GOItem *)item;NSError *error=nil;size_t total=0;if([self ensureOpen:i modes:FSVolumeOpenModesWrite error:&error])while(total<contents.length){GOReadWrite input={.handle=i.handle,.offset=offset+(off_t)total,.size=(uint32_t)MIN(contents.length-total,512*1024),.flags=O_RDWR};NSMutableData *body=[bytes(&input,sizeof(input)) mutableCopy];[body appendData:[contents subdataWithRange:NSMakeRange(total,input.size)]];NSData *data=[self request:16 node:i.inode body:body error:&error];if(data.length<8){if(!error)error=failure(EPROTO);break;}uint32_t written;memcpy(&written,data.bytes,4);if(!written || written>input.size){error=failure(EIO);break;}total+=written;}reply(total,error);}
}
- (void)synchronizeWithFlags:(FSSyncFlags)flags replyHandler:(void(^)(NSError *))reply {
    @synchronized(self){NSError *error=nil;for(GOItem *item in _items.allValues)if(item.handle){uint64_t body[]={item.handle,0};if(![self request:20 node:item.inode body:bytes(body,sizeof(body)) error:&error])break;}reply(error);}
}
- (void)checkAccessToItem:(FSItem *)item requestedAccess:(FSAccessMask)access replyHandler:(void(^)(BOOL,NSError *))reply {
    @synchronized(self){NSError *error=nil;FSItemAttributes *a=[self attributesOf:(GOItem *)item error:&error];BOOL allowed=a!=nil;
    if(access&(FSAccessReadData|FSAccessListDirectory))allowed&=(a.mode&0400)!=0;
    if(access&(FSAccessWriteData|FSAccessAppendData|FSAccessDeleteChild))allowed&=(a.mode&0200)!=0;
    if(access&(FSAccessExecute|FSAccessSearch))allowed&=(a.mode&0100)!=0;
    // Policy authorization is checked again by the shared Rust operation itself.
    reply(allowed,error);}
}
@end
@implementation GitOnFileSystem
- (void)probeResource:(FSResource *)resource replyHandler:(void(^)(FSProbeResult *,NSError *))reply {
    if(![resource isKindOfClass:FSPathURLResource.class]){reply(FSProbeResult.notRecognizedProbeResult,nil);return;}
    NSURL *url=((FSPathURLResource *)resource).url;[url startAccessingSecurityScopedResource];struct stat st;BOOL usable=lstat([[url URLByAppendingPathComponent:@"bridge.sock"] fileSystemRepresentation],&st)==0 && S_ISSOCK(st.st_mode) && st.st_uid==geteuid();[url stopAccessingSecurityScopedResource];
    reply(usable?[FSProbeResult usableProbeResultWithName:@"Git-on" containerID:[FSContainerIdentifier new]]:FSProbeResult.notRecognizedProbeResult,nil);
}
- (void)loadResource:(FSResource *)resource options:(FSTaskOptions *)options replyHandler:(void(^)(FSVolume *,NSError *))reply {
    if(![resource isKindOfClass:FSPathURLResource.class]){reply(nil,failure(EINVAL));return;}
    NSError *error=nil;GOVolume *volume=[[GOVolume alloc] initWithURL:((FSPathURLResource *)resource).url error:&error];if(volume)self.containerStatus=FSContainerStatus.ready;reply(volume,error);
}
- (void)unloadResource:(FSResource *)resource options:(FSTaskOptions *)options replyHandler:(void(^)(NSError *))reply{reply(nil);}
@end
