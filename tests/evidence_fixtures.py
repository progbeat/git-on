"""Synthetic validator inputs, never evidence of an actual execution."""


def pack_scaling_rows(head):
    return [dict(head=head,working_tree_dirty=False,distinct_blobs=n,passes=2,baseline=False,
                 metrics=dict(remote_fetches=n+2),pack_metadata_syscalls=n,
                 pack_directory_bytes_enumerated=n*64) for n in (32,128,512)]


def object_reuse_row(head,backend):
    count=160 if backend=='fuse' else 32
    operations=count+2 if backend=='fuse' else 3
    decodes=operations if backend=='fuse' else 2
    raw_count=count if backend=='fuse' else 0
    return dict(head=head,working_tree_dirty=False,backend=backend,type='runner',files=count,
        blob_bytes=8192,stat_abi_bytes=144,
        child_observations=[dict(completed=p,files=count,payload_bytes=count*8192,
                                metadata_result_bytes=count*144) for p in ('first','repeat')],
        metrics=dict(remote_fetches=count+2,remote_fetch_operations=operations,remote_decode_operations=decodes,
                     remote_raw_blob_imports=raw_count,remote_raw_blob_bytes=raw_count*8192,blob_loads=count,
                     blob_cache_bytes=1,blob_cache_limit=16*1024*1024),
        fuse_requests={'read':1} if backend=='fuse' else {},
        native_commands={'cat-file':decodes,'fetch':operations})
