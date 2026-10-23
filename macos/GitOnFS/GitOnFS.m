#import "GitOnFS.h"
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>

// The app extension translates Apple operations only. Git bytes, namespace,
// policy decisions and backing handles remain in the shared Rust engine.
static NSError *failure(int code) {
    return [NSError errorWithDomain:NSPOSIXErrorDomain code:code userInfo:nil];
}
static BOOL transfer(int fd, void *buffer, size_t length, BOOL writing) {
    uint8_t *cursor=buffer;
    while(length) {
        ssize_t count=writing?write(fd,cursor,length):read(fd,cursor,length);
        if(count<0 && errno==EINTR)continue;
        if(count<=0)return NO;
        cursor+=count;length-=count;
    }
    return YES;
}
static id jsonValue(id object) {
    if([object isKindOfClass:NSData.class]) {
        NSData *data=object;NSMutableArray *values=[NSMutableArray arrayWithCapacity:data.length];
        const uint8_t *bytes=data.bytes;
        for(NSUInteger i=0;i<data.length;i++)[values addObject:@(bytes[i])];
        return values;
    }
    if([object isKindOfClass:NSDictionary.class]) {
        NSMutableDictionary *values=[NSMutableDictionary new];
        for(id key in object)values[key]=jsonValue(object[key]);
        return values;
    }
    return object;
}
static NSData *dataValue(id object, NSError **error) {
    if(![object isKindOfClass:NSArray.class] || [object count]>512*1024) {
        *error=failure(EPROTO);return nil;
    }
    NSMutableData *data=[NSMutableData dataWithLength:[object count]];uint8_t *bytes=data.mutableBytes;
    for(NSUInteger i=0;i<[object count];i++) {
        id value=object[i];
        if(![value isKindOfClass:NSNumber.class] || [value longLongValue]<0 || [value longLongValue]>255 || [value doubleValue]!=[value longLongValue]) {
            *error=failure(EPROTO);return nil;
        }
        bytes[i]=[value unsignedCharValue];
    }
    return data;
}
static FSItemType itemType(NSString *kind) {
    if([kind isEqual:@"Directory"])return FSItemTypeDirectory;
    if([kind isEqual:@"Symlink"])return FSItemTypeSymlink;
    return FSItemTypeFile;
}
static struct timespec timestamp(NSDictionary *time) {
    return (struct timespec){[time[@"secs_since_epoch"] longLongValue],[time[@"nanos_since_epoch"] longValue]};
}
static NSDictionary *timeValue(struct timespec time) {
    return @{@"secs":@(time.tv_sec),@"nanos":@(time.tv_nsec)};
}
@interface GOItem : FSItem
@property uint64_t inode,references,handle;
@property int flags;
@property FSItemType kind;
@end
@implementation GOItem
@end

@interface GOVolume : FSVolume <FSVolumeOperations,FSVolumeOpenCloseOperations,FSVolumeReadWriteOperations,FSVolumeAccessCheckOperations>
@property int socketFD;
@property uint64_t namespaceVersion;
@property NSURL *resourceURL;
@property BOOL readOnly;
@property NSMutableDictionary<NSNumber *,GOItem *> *items;
- (instancetype)initWithURL:(NSURL *)url error:(NSError **)error;
@end
@implementation GOVolume
- (instancetype)initWithURL:(NSURL *)url error:(NSError **)error {
    self=[super initWithVolumeID:[FSVolumeIdentifier new] volumeName:[[FSFileName alloc] initWithString:@"Git-on"]];
    if(!self)return nil;
    _socketFD=-1;_resourceURL=url;_namespaceVersion=1;_items=[NSMutableDictionary new];
    [url startAccessingSecurityScopedResource];
    struct stat info;
    if(lstat(url.fileSystemRepresentation,&info) || !S_ISDIR(info.st_mode) || info.st_uid!=geteuid() || (info.st_mode&077)!=0) {
        *error=failure(EACCES);return nil;
    }
    NSURL *endpoint=[url URLByAppendingPathComponent:@"bridge.sock"];
    if(lstat(endpoint.fileSystemRepresentation,&info) || !S_ISSOCK(info.st_mode) || info.st_uid!=geteuid() || (info.st_mode&077)!=0) {
        *error=failure(EACCES);return nil;
    }
    struct sockaddr_un address={.sun_family=AF_UNIX};
    if(strlen(endpoint.fileSystemRepresentation)>=sizeof(address.sun_path)){*error=failure(ENAMETOOLONG);return nil;}
    strlcpy(address.sun_path,endpoint.fileSystemRepresentation,sizeof(address.sun_path));
    _socketFD=socket(AF_UNIX,SOCK_STREAM,0);
    if(_socketFD<0 || connect(_socketFD,(struct sockaddr *)&address,sizeof(address))) {*error=failure(errno);return nil;}
    fcntl(_socketFD,F_SETFD,FD_CLOEXEC);
    uid_t uid;gid_t gid;
    if(getpeereid(_socketFD,&uid,&gid) || uid!=geteuid()){*error=failure(EACCES);return nil;}
    int one=1;setsockopt(_socketFD,SOL_SOCKET,SO_NOSIGPIPE,&one,sizeof(one));
    struct timeval timeout={.tv_sec=60};
    setsockopt(_socketFD,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout));
    setsockopt(_socketFD,SOL_SOCKET,SO_SNDTIMEO,&timeout,sizeof(timeout));
    NSDictionary *result=[self request:@"initialize" arguments:@{} error:error];
    if(!result || [result[@"protocol"] intValue]!=1){if(!*error)*error=failure(EPROTO);return nil;}
    _readOnly=[result[@"read_only"] boolValue];return self;
}
- (void)dealloc {if(_socketFD>=0)close(_socketFD);[_resourceURL stopAccessingSecurityScopedResource];}
- (id)request:(NSString *)operation arguments:(NSDictionary *)arguments error:(NSError **)error {
    @synchronized(self) {
        NSMutableDictionary *message=[jsonValue(arguments) mutableCopy];message[@"op"]=operation;
        NSData *input=[NSJSONSerialization dataWithJSONObject:message options:0 error:error];
        if(!input || input.length>8*1024*1024){if(!*error)*error=failure(E2BIG);return nil;}
        uint32_t length=(uint32_t)input.length;
        if(!transfer(_socketFD,&length,4,YES) || !transfer(_socketFD,(void *)input.bytes,input.length,YES) || !transfer(_socketFD,&length,4,NO)) {
            *error=failure(errno?:EIO);return nil;
        }
        if(!length || length>8*1024*1024){shutdown(_socketFD,SHUT_RDWR);*error=failure(EPROTO);return nil;}
        NSMutableData *output=[NSMutableData dataWithLength:length];
        if(!transfer(_socketFD,output.mutableBytes,length,NO)){*error=failure(errno?:EIO);return nil;}
        id response=[NSJSONSerialization JSONObjectWithData:output options:NSJSONReadingFragmentsAllowed error:error];
        if(![response isKindOfClass:NSDictionary.class]){*error=failure(EPROTO);return nil;}
        if(response[@"error"]){*error=failure([response[@"error"] intValue]);return nil;}
        if(!response[@"ok"]){*error=failure(EPROTO);return nil;}
        return response[@"ok"];
    }
}
- (GOItem *)item:(uint64_t)inode kind:(FSItemType)kind retainLookup:(BOOL)retainLookup {
    GOItem *item=_items[@(inode)];
    if(!item){item=[GOItem new];item.inode=inode;item.kind=kind;_items[@(inode)]=item;}
    if(retainLookup)item.references++;return item;
}
- (GOItem *)entry:(NSDictionary *)attributes {
    return [self item:[attributes[@"ino"] unsignedLongLongValue] kind:itemType(attributes[@"kind"]) retainLookup:YES];
}
- (FSItemAttributes *)attributes:(NSDictionary *)a {
    FSItemAttributes *result=[FSItemAttributes new];
    result.type=itemType(a[@"kind"]);result.mode=[a[@"perm"] unsignedIntValue];result.uid=[a[@"uid"] unsignedIntValue];result.gid=[a[@"gid"] unsignedIntValue];
    result.linkCount=[a[@"nlink"] unsignedIntValue];result.size=[a[@"size"] unsignedLongLongValue];result.allocSize=[a[@"blocks"] unsignedLongLongValue]*512;
    result.fileID=(FSItemID)([a[@"ino"] unsignedLongLongValue]+1);
    result.accessTime=timestamp(a[@"atime"]);result.modifyTime=timestamp(a[@"mtime"]);result.changeTime=timestamp(a[@"ctime"]);result.birthTime=timestamp(a[@"crtime"]);result.flags=0;
    return result;
}
- (NSInteger)maximumLinkCount{return UINT32_MAX;}
- (NSInteger)maximumNameLength{return 255;}
- (BOOL)restrictsOwnershipChanges{return YES;}
- (BOOL)truncatesLongNames{return NO;}
- (NSInteger)maximumFileSizeInBits{return 64;}
- (NSInteger)maximumXattrSize{return 0;}
- (FSVolumeSupportedCapabilities *)supportedVolumeCapabilities {
    FSVolumeSupportedCapabilities *capabilities=[FSVolumeSupportedCapabilities new];
    capabilities.supportsSymbolicLinks=YES;capabilities.supportsHardLinks=YES;capabilities.caseFormat=FSVolumeCaseFormatSensitive;return capabilities;
}
- (FSStatFSResult *)volumeStatistics {
    @synchronized(self) {
        NSError *error=nil;NSDictionary *a=[self request:@"statfs" arguments:@{} error:&error];
        FSStatFSResult *result=[[FSStatFSResult alloc] initWithFileSystemTypeName:@"giton"];
        if(a){result.blockSize=[a[@"block_size"] unsignedLongLongValue];result.ioSize=result.blockSize;result.totalBlocks=[a[@"blocks"] unsignedLongLongValue];result.freeBlocks=[a[@"free"] unsignedLongLongValue];result.availableBlocks=[a[@"available"] unsignedLongLongValue];result.totalFiles=[a[@"files"] unsignedLongLongValue];result.freeFiles=[a[@"free_files"] unsignedLongLongValue];}
        return result;
    }
}
- (FSMountOptions)requestedMountOptions{return _readOnly?FSMountOptionsReadOnly:0;}
- (void)mountWithOptions:(FSTaskOptions *)options replyHandler:(void(^)(NSError *))reply{reply(nil);}
- (void)unmountWithReplyHandler:(void(^)(void))reply{shutdown(_socketFD,SHUT_RDWR);reply();}
- (void)activateWithOptions:(FSTaskOptions *)options replyHandler:(void(^)(FSItem *,NSError *))reply{@synchronized(self){reply([self item:1 kind:FSItemTypeDirectory retainLookup:NO],nil);}}
- (void)deactivateWithOptions:(FSDeactivateOptions)options replyHandler:(void(^)(NSError *))reply{reply(nil);}
- (void)getAttributes:(FSItemGetAttributesRequest *)desired ofItem:(FSItem *)item replyHandler:(void(^)(FSItemAttributes *,NSError *))reply {
    @synchronized(self){NSError *error=nil;NSDictionary *a=[self request:@"getattr" arguments:@{@"ino":@(((GOItem *)item).inode)} error:&error];reply(a?[self attributes:a]:nil,error);}
}
- (void)setAttributes:(FSItemSetAttributesRequest *)attributes onItem:(FSItem *)item replyHandler:(void(^)(FSItemAttributes *,NSError *))reply {
    @synchronized(self) {
        NSMutableDictionary *update=[NSMutableDictionary new];FSItemAttribute consumed=0;
        const FSItemAttribute fields[]={FSItemAttributeMode,FSItemAttributeUID,FSItemAttributeGID,FSItemAttributeSize,FSItemAttributeAccessTime,FSItemAttributeModifyTime,FSItemAttributeBirthTime,FSItemAttributeFlags};
        NSArray *keys=@[@"mode",@"uid",@"gid",@"size",@"atime",@"mtime",@"crtime",@"flags"];
        NSArray *values=@[@(attributes.mode),@(attributes.uid),@(attributes.gid),@(attributes.size),timeValue(attributes.accessTime),timeValue(attributes.modifyTime),timeValue(attributes.birthTime),@(attributes.flags)];
        for(NSUInteger i=0;i<8;i++)if([attributes isValid:fields[i]]){update[keys[i]]=values[i];consumed|=fields[i];}
        NSError *error=nil;NSDictionary *a=[self request:@"setattr" arguments:@{@"ino":@(((GOItem *)item).inode),@"attributes":update} error:&error];
        if(a)attributes.consumedAttributes=consumed;reply(a?[self attributes:a]:nil,error);
    }
}
- (void)lookupItemNamed:(FSFileName *)name inDirectory:(FSItem *)directory replyHandler:(void(^)(FSItem *,FSFileName *,NSError *))reply {
    @synchronized(self){NSError *error=nil;NSDictionary *a=[self request:@"lookup" arguments:@{@"parent":@(((GOItem *)directory).inode),@"name":name.data} error:&error];reply(a?[self entry:a]:nil,a?name:nil,error);}
}
- (void)reclaimItem:(FSItem *)item replyHandler:(void(^)(NSError *))reply {
    @synchronized(self){GOItem *i=(GOItem *)item;NSError *error=nil;
        if(i.handle)[self request:@"close" arguments:@{@"ino":@(i.inode),@"handle":@(i.handle)} error:&error];
        if(!error && i.references)[self request:@"forget" arguments:@{@"ino":@(i.inode),@"count":@(i.references)} error:&error];
        if(!error)[_items removeObjectForKey:@(i.inode)];reply(error);
    }
}
- (void)readSymbolicLink:(FSItem *)item replyHandler:(void(^)(FSFileName *,NSError *))reply {
    @synchronized(self){NSError *error=nil;id result=[self request:@"readlink" arguments:@{@"ino":@(((GOItem *)item).inode)} error:&error];NSData *data=result?dataValue(result,&error):nil;reply(data?[[FSFileName alloc] initWithData:data]:nil,error);}
}
- (void)createItemNamed:(FSFileName *)name type:(FSItemType)type inDirectory:(FSItem *)directory attributes:(FSItemSetAttributesRequest *)attributes replyHandler:(void(^)(FSItem *,FSFileName *,NSError *))reply {
    @synchronized(self){if(type!=FSItemTypeFile && type!=FSItemTypeDirectory){reply(nil,nil,failure(ENOTSUP));return;}
        NSError *error=nil;NSDictionary *a=[self request:@"create" arguments:@{@"parent":@(((GOItem *)directory).inode),@"name":name.data,@"kind":type==FSItemTypeDirectory?@"directory":@"file",@"mode":@(attributes.mode)} error:&error];
        if(a)_namespaceVersion++;reply(a?[self entry:a]:nil,a?name:nil,error);
    }
}
- (void)createSymbolicLinkNamed:(FSFileName *)name inDirectory:(FSItem *)directory attributes:(FSItemSetAttributesRequest *)attributes linkContents:(FSFileName *)contents replyHandler:(void(^)(FSItem *,FSFileName *,NSError *))reply {
    @synchronized(self){NSError *error=nil;NSDictionary *a=[self request:@"symlink" arguments:@{@"parent":@(((GOItem *)directory).inode),@"name":name.data,@"target":contents.data} error:&error];if(a)_namespaceVersion++;reply(a?[self entry:a]:nil,a?name:nil,error);}
}
- (void)createLinkToItem:(FSItem *)item named:(FSFileName *)name inDirectory:(FSItem *)directory replyHandler:(void(^)(FSFileName *,NSError *))reply {
    @synchronized(self){NSError *error=nil;NSDictionary *a=[self request:@"link" arguments:@{@"ino":@(((GOItem *)item).inode),@"parent":@(((GOItem *)directory).inode),@"name":name.data} error:&error];if(a){[self entry:a];_namespaceVersion++;}reply(a?name:nil,error);}
}
- (void)removeItem:(FSItem *)item named:(FSFileName *)name fromDirectory:(FSItem *)directory replyHandler:(void(^)(NSError *))reply {
    @synchronized(self){NSError *error=nil;id result=[self request:@"remove" arguments:@{@"parent":@(((GOItem *)directory).inode),@"name":name.data,@"directory":@(((GOItem *)item).kind==FSItemTypeDirectory)} error:&error];if(result)_namespaceVersion++;reply(error);}
}
- (void)renameItem:(FSItem *)item inDirectory:(FSItem *)source named:(FSFileName *)name toNewName:(FSFileName *)newName inDirectory:(FSItem *)destination overItem:(FSItem *)over replyHandler:(void(^)(FSFileName *,NSError *))reply {
    @synchronized(self){NSError *error=nil;id result=[self request:@"rename" arguments:@{@"parent":@(((GOItem *)source).inode),@"name":name.data,@"newparent":@(((GOItem *)destination).inode),@"newname":newName.data} error:&error];if(result)_namespaceVersion++;reply(result?newName:nil,error);}
}
- (void)enumerateDirectory:(FSItem *)directory startingAtCookie:(FSDirectoryCookie)cookie verifier:(FSDirectoryVerifier)verifier providingAttributes:(FSItemGetAttributesRequest *)attributes usingPacker:(FSDirectoryEntryPacker *)packer replyHandler:(void(^)(FSDirectoryVerifier,NSError *))reply {
    @synchronized(self){if(verifier && verifier!=_namespaceVersion){reply(0,failure(EINVAL));return;}
        GOItem *dir=(GOItem *)directory;NSError *error=nil;NSNumber *handle=[self request:@"open" arguments:@{@"ino":@(dir.inode),@"flags":@(O_RDONLY),@"directory":@YES} error:&error];
        if(!handle){reply(0,error);return;}
        NSArray *entries=[self request:@"read_dir" arguments:@{@"ino":@(dir.inode),@"handle":handle,@"offset":@(cookie)} error:&error];
        for(NSDictionary *entry in entries){NSData *data=dataValue(entry[@"name"],&error);if(!data)break;
            if(attributes && ([data isEqual:[NSData dataWithBytes:"." length:1]] || [data isEqual:[NSData dataWithBytes:".." length:2]]))continue;
            FSItemAttributes *a=nil;
            if(attributes){NSDictionary *result=[self request:@"getattr" arguments:@{@"ino":entry[@"ino"]} error:&error];if(!result)break;a=[self attributes:result];}
            if(![packer packEntryWithName:[[FSFileName alloc] initWithData:data] itemType:itemType(entry[@"kind"]) itemID:(FSItemID)([entry[@"ino"] unsignedLongLongValue]+1) nextCookie:[entry[@"next"] unsignedLongLongValue] attributes:a])break;
        }
        NSError *closeError=nil;[self request:@"close" arguments:@{@"ino":@(dir.inode),@"handle":handle} error:&closeError];reply(_namespaceVersion,error?:closeError);
    }
}
- (BOOL)ensureOpen:(GOItem *)item modes:(FSVolumeOpenModes)modes error:(NSError **)error {
    int flags=(modes&FSVolumeOpenModesWrite)?O_RDWR:O_RDONLY;
    if(item.handle && (item.flags==O_RDWR || flags==O_RDONLY))return YES;
    NSNumber *handle=[self request:@"open" arguments:@{@"ino":@(item.inode),@"flags":@(flags),@"directory":@(item.kind==FSItemTypeDirectory)} error:error];
    if(!handle)return NO;
    if(item.handle){[self request:@"close" arguments:@{@"ino":@(item.inode),@"handle":@(item.handle)} error:error];if(*error)return NO;}
    item.handle=handle.unsignedLongLongValue;item.flags=flags;return YES;
}
- (void)openItem:(FSItem *)item withModes:(FSVolumeOpenModes)modes replyHandler:(void(^)(NSError *))reply {
    @synchronized(self){NSError *error=nil;[self ensureOpen:(GOItem *)item modes:modes error:&error];reply(error);}
}
- (void)closeItem:(FSItem *)item keepingModes:(FSVolumeOpenModes)modes replyHandler:(void(^)(NSError *))reply {
    @synchronized(self){GOItem *i=(GOItem *)item;NSError *error=nil;
        if(!modes && i.handle){[self request:@"close" arguments:@{@"ino":@(i.inode),@"handle":@(i.handle)} error:&error];if(!error)i.handle=0;}reply(error);
    }
}
- (void)readFromFile:(FSItem *)item offset:(off_t)offset length:(size_t)length intoBuffer:(FSMutableFileDataBuffer *)buffer replyHandler:(void(^)(size_t,NSError *))reply {
    @synchronized(self){GOItem *i=(GOItem *)item;NSError *error=nil;size_t total=0;memset(buffer.mutableBytes,0,buffer.length);
        if([self ensureOpen:i modes:FSVolumeOpenModesRead error:&error]){while(total<length){size_t count=MIN(length-total,512*1024);
            id result=[self request:@"read" arguments:@{@"ino":@(i.inode),@"handle":@(i.handle),@"offset":@(offset+(off_t)total),@"size":@(count)} error:&error];
            NSData *data=result?dataValue(result,&error):nil;if(!data)break;
            if(data.length>count || total+data.length>buffer.length){error=failure(EPROTO);break;}
            memcpy((uint8_t *)buffer.mutableBytes+total,data.bytes,data.length);total+=data.length;if(data.length<count)break;
        }}
        reply(total,error);
    }
}
- (void)writeContents:(NSData *)contents toFile:(FSItem *)item atOffset:(off_t)offset replyHandler:(void(^)(size_t,NSError *))reply {
    @synchronized(self){GOItem *i=(GOItem *)item;NSError *error=nil;size_t total=0;
        if([self ensureOpen:i modes:FSVolumeOpenModesWrite error:&error]){while(total<contents.length){size_t count=MIN(contents.length-total,512*1024);
            NSNumber *written=[self request:@"write" arguments:@{@"ino":@(i.inode),@"handle":@(i.handle),@"offset":@(offset+(off_t)total),@"data":[contents subdataWithRange:NSMakeRange(total,count)]} error:&error];
            if(!written)break;if(written.unsignedLongLongValue==0 || written.unsignedLongLongValue>count){error=failure(EIO);break;}total+=written.unsignedLongLongValue;
        }}
        reply(total,error);
    }
}
- (void)synchronizeWithFlags:(FSSyncFlags)flags replyHandler:(void(^)(NSError *))reply {
    @synchronized(self){NSError *error=nil;
        for(GOItem *item in _items.allValues)if(item.handle && ![self request:@"sync" arguments:@{@"ino":@(item.inode),@"handle":@(item.handle),@"directory":@(item.kind==FSItemTypeDirectory)} error:&error])break;
        reply(error);
    }
}
- (void)checkAccessToItem:(FSItem *)item requestedAccess:(FSAccessMask)access replyHandler:(void(^)(BOOL,NSError *))reply {
    @synchronized(self){int mask=0;if(access&FSAccessReadData)mask|=R_OK;if(access&(FSAccessWriteData|FSAccessAppendData|FSAccessDeleteChild))mask|=W_OK;if(access&FSAccessExecute)mask|=X_OK;
        NSError *error=nil;id result=[self request:@"access" arguments:@{@"ino":@(((GOItem *)item).inode),@"mask":@(mask)} error:&error];
        if(error.code==EACCES || error.code==EROFS)reply(NO,nil);else reply(result!=nil,error);
    }
}
@end
@implementation GitOnFileSystem
- (void)probeResource:(FSResource *)resource replyHandler:(void(^)(FSProbeResult *,NSError *))reply {
    if(![resource isKindOfClass:FSPathURLResource.class]){reply(FSProbeResult.notRecognizedProbeResult,nil);return;}
    NSURL *url=((FSPathURLResource *)resource).url;[url startAccessingSecurityScopedResource];struct stat info;
    BOOL usable=lstat([[url URLByAppendingPathComponent:@"bridge.sock"] fileSystemRepresentation],&info)==0 && S_ISSOCK(info.st_mode) && info.st_uid==geteuid();[url stopAccessingSecurityScopedResource];
    reply(usable?[FSProbeResult usableProbeResultWithName:@"Git-on" containerID:[FSContainerIdentifier new]]:FSProbeResult.notRecognizedProbeResult,nil);
}
- (void)loadResource:(FSResource *)resource options:(FSTaskOptions *)options replyHandler:(void(^)(FSVolume *,NSError *))reply {
    if(![resource isKindOfClass:FSPathURLResource.class]){reply(nil,failure(EINVAL));return;}
    NSError *error=nil;GOVolume *volume=[[GOVolume alloc] initWithURL:((FSPathURLResource *)resource).url error:&error];
    if(volume)self.containerStatus=FSContainerStatus.ready;reply(volume,error);
}
- (void)unloadResource:(FSResource *)resource options:(FSTaskOptions *)options replyHandler:(void(^)(NSError *))reply{reply(nil);}
@end
