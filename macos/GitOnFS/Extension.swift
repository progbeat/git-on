import FSKit
@main
struct GitOnExtension: UnaryFileSystemExtension {
    var fileSystem: GitOnFileSystem { GitOnFileSystem() }
}
