What etcd's own proto files import from elsewhere, here so generating `src/etcdpb` needs
protoc alone. Not compared by `.github/proto-compare.sh`, which covers `proto/etcd/` only:

- `google/api/` - [googleapis](https://github.com/googleapis/googleapis), Apache-2.0
- `protoc-gen-openapiv2/options/` - [grpc-gateway](https://github.com/grpc-ecosystem/grpc-gateway)
  v2.29.0, as etcd 3.7.3 pins it, BSD-3-Clause
- `google/protobuf/` - [protobuf](https://github.com/protocolbuffers/protobuf) 3.21.12, BSD-3-Clause
