"""Synthetic validator inputs, never evidence of an actual execution."""


def pack_scaling_rows(head):
    return [dict(head=head,working_tree_dirty=False,distinct_blobs=n,passes=2,baseline=False,
                 metrics=dict(remote_fetches=n+2),pack_metadata_syscalls=0,
                 pack_directory_bytes_enumerated=0) for n in (32,128,512)]


def object_reuse_row(head,backend):
    count=160 if backend=='fuse' else 32
    operations=count+2 if backend=='fuse' else 1
    return dict(head=head,working_tree_dirty=False,backend=backend,type='runner',files=count,
        blob_bytes=8192,stat_abi_bytes=144,
        child_observations=[dict(completed=p,files=count,payload_bytes=count*8192,
                                metadata_result_bytes=count*144) for p in ('first','repeat')],
        metrics=dict(remote_fetches=count+2 if backend=='fuse' else 1,remote_fetch_operations=0,remote_decode_operations=0,remote_native_pack_bytes=0,
                     remote_http2_fetch_operations=operations,remote_session_fetch_operations=operations,
                     remote_session_decode_operations=operations,remote_session_starts=1,
                     remote_raw_blob_imports=count,remote_raw_blob_bytes=count*8192,blob_loads=count,
                     blob_cache_bytes=1,blob_cache_limit=16*1024*1024),
        fuse_requests={'read':1} if backend=='fuse' else {},
        native_commands={})
