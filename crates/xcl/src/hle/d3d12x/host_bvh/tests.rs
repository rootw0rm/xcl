use super::*;
use std::mem::size_of_val;

#[test]
fn captured_descriptors_follow_the_observed_abi() {
    assert_eq!(size_of::<BatchedDesc>(), 80);
    assert_eq!(size_of::<D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC>(), 48);
    assert_eq!(std::mem::offset_of!(BatchedDesc, postbuild_count), 48);
    assert_eq!(std::mem::offset_of!(BatchedDesc, postbuild), 56);
    assert_eq!(size_of::<D3D12_RAYTRACING_GEOMETRY_DESC>(), 56);
}

#[test]
fn descriptor_capture_checks_spans_and_preserves_records() {
    unsafe {
        let data = [17u64, 29];
        assert_eq!(read_items::<u64>(data.as_ptr() as usize, 2).unwrap(), data);
        assert!(read_items::<u64>(0, 1).is_err());
        assert!(read_items::<u64>(usize::MAX - 2, 2).is_err());
        assert!(read_items::<u64>(data.as_ptr() as usize, MAX_DESCS + 1).is_err());
        assert!(read_items::<u64>(0, 0).unwrap().is_empty());
    }
}

#[test]
fn unsupported_exports_keep_the_native_target() {
    assert!(replacement("BuildRaytracingAccelerationStructure", 1).is_none());
    assert!(replacement("?Unrelated@XboxBvhBuilder@@", 1).is_none());
}

#[test]
fn packed_vertex_spans_exclude_unused_alpha_and_check_overflow() {
    let mut triangle = D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
        VertexFormat: DXGI_FORMAT_R16G16B16A16_FLOAT,
        VertexCount: 3,
        VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE { StartAddress: 256, StrideInBytes: 6 },
        ..Default::default()
    };
    assert_eq!(vertex_span(&triangle).unwrap(), 18);
    triangle.VertexCount = 0;
    assert!(vertex_span(&triangle).is_err());
    triangle.VertexCount = 3;
    triangle.VertexBuffer.StrideInBytes = u64::MAX;
    assert!(vertex_span(&triangle).is_err());
}

#[test]
fn hooks_forward_arguments_and_preserve_rehydrate_failure() {
    unsafe extern "system-unwind" fn original_build(this: *mut c_void, list: *mut c_void, count: u32, descs: *const BatchedDesc) {
        assert_eq!(list as usize, 17);
        assert_eq!(count, 0);
        assert!(descs.is_null());
        *(this as *mut u32) += 1;
    }
    unsafe extern "system-unwind" fn original_rehydrate(this: *mut c_void, list: *mut c_void, count: u32,
        descs: *const D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC, sources: *const u64) -> i32 {
        assert_eq!(list as usize, 29);
        assert_eq!(count, 3);
        assert_eq!(descs as usize, 41);
        assert_eq!(sources as usize, 53);
        *(this as *mut u32) += 1;
        -7
    }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            BUILD_FN.store(0, Ordering::Release);
            REHYDRATE_FN.store(0, Ordering::Release);
        }
    }
    let _reset = Reset;
    assert_eq!(replacement(BUILD, original_build as usize), Some(build as usize));
    assert_eq!(replacement(BUILD, original_build as usize), Some(build as usize));
    assert_eq!(replacement(BUILD, 1), None);
    assert_eq!(replacement(REHYDRATE, original_rehydrate as usize), Some(rehydrate as usize));
    let mut calls = 0u32;
    unsafe {
        let this = (&mut calls as *mut u32).cast();
        build(this, 17usize as *mut c_void, 0, std::ptr::null());
        assert_eq!(rehydrate(this, 29usize as *mut c_void, 3, 41usize as *const _, 53usize as *const _), -7);
    }
    assert_eq!(calls, 2);
}

#[test]
fn gpu_shadow_dxr_builds_and_clones_blas_then_traces_a_tlas() {
    use crate::gpu::recomp::{dxc, runner};
    use windows::Win32::Graphics::Direct3D::*;
    use windows::Win32::Graphics::Dxgi::*;

    unsafe {
        if dxc::location().is_err() {
            runner::skip_test("no DXC");
            return;
        }
        let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory6>(DXGI_CREATE_FACTORY_FLAGS(0)) else {
            runner::skip_test("no DXGI factory");
            return;
        };
        let selector = crate::gpu::adapter::Selector::from_env_or("auto");
        let Some((adapter, desc)) = crate::gpu::adapter::candidates(&factory, &selector).into_iter().next() else {
            runner::skip_test("no GPU");
            return;
        };
        let Ok(base) = crate::gpu::agility::create_device(&adapter, D3D_FEATURE_LEVEL_12_1) else {
            runner::skip_test("no D3D12 device");
            return;
        };
        let Ok(dev) = base.cast::<ID3D12Device5>() else {
            runner::skip_test("no DXR interface");
            return;
        };
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS5::default();
        dev.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS5, (&mut options as *mut D3D12_FEATURE_DATA_D3D12_OPTIONS5).cast(), size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS5>() as u32).unwrap();
        if options.RaytracingTier.0 < D3D12_RAYTRACING_TIER_1_1.0 {
            runner::skip_test("no DXR 1.1");
            return;
        }
        println!("Host DXR fixture on {}", crate::gpu::adapter::name(&desc));
        let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
        let allocator: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let list: ID3D12GraphicsCommandList4 = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
        let mut allocations = Prepared::new(LIMIT);
        let vertices = [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let vertex_bytes = std::slice::from_raw_parts(vertices.as_ptr().cast::<u8>(), size_of_val(&vertices));
        let upload = allocations.buffer(&dev, vertex_bytes.len() as u64, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ).unwrap();
        let mut mapped = std::ptr::null_mut();
        upload.Map(0, None, Some(&mut mapped)).unwrap();
        std::ptr::copy_nonoverlapping(vertex_bytes.as_ptr(), mapped.cast(), vertex_bytes.len());
        upload.Unmap(0, None);
        let vertex_buffer = allocations.buffer(&dev, vertex_bytes.len() as u64, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COPY_DEST).unwrap();
        list.CopyBufferRegion(&vertex_buffer, 0, &upload, 0, vertex_bytes.len() as u64);
        list.ResourceBarrier(&[transition(&vertex_buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);

        let console_vertices = 0x100000u64;
        let uma = crate::gpu::uma::get().unwrap();
        let span = uma.alloc(3 * 65536, 65536).unwrap() as usize;
        struct Span(usize);
        impl Drop for Span {
            fn drop(&mut self) {
                forget_range(self.0, 3 * 65536);
                crate::gpu::uma::get().unwrap().free(self.0);
            }
        }
        let _span = Span(span);
        let console_blas = span as u64;
        let console_clone = console_blas + 65536;
        let console_tlas = console_clone + 65536;
        let geometry = D3D12_RAYTRACING_GEOMETRY_DESC {
            Type: D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES,
            Flags: D3D12_RAYTRACING_GEOMETRY_FLAG_OPAQUE,
            Anonymous: D3D12_RAYTRACING_GEOMETRY_DESC_0 {
                Triangles: D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
                    VertexFormat: DXGI_FORMAT_R32G32B32_FLOAT,
                    VertexCount: 3,
                    VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE { StartAddress: console_vertices, StrideInBytes: 12 },
                    ..Default::default()
                },
            },
        };
        let bottom = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
            DestAccelerationStructureData: console_blas,
            Inputs: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
                Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL,
                NumDescs: 1, DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
                Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 { pGeometryDescs: &geometry },
                ..Default::default()
            },
            ..Default::default()
        };
        let source = |address, bytes| {
            (address == console_vertices && bytes <= vertex_bytes.len() as u64).then(|| (vertex_buffer.clone(), 0))
        };
        let mut state = State::default();
        let blas = record_on(&mut state, &dev, &list, &bottom, &source).unwrap();
        assert_eq!(state.structures.len(), 1);
        let cloned = record_copy_on(&mut state, &dev, &list, console_clone, console_blas,
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_COMPACT.0).unwrap();
        assert_ne!(blas.resource.GetGPUVirtualAddress(), cloned.resource.GetGPUVirtualAddress());

        let mut instance = [0u8; 64];
        for i in 0..3 {
            instance[(i * 4 + i) * 4..(i * 4 + i + 1) * 4].copy_from_slice(&1.0f32.to_le_bytes());
        }
        instance[48..52].copy_from_slice(&(0xffu32 << 24).to_le_bytes());
        instance[56..64].copy_from_slice(&console_clone.to_le_bytes());
        let top = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
            DestAccelerationStructureData: console_tlas,
            Inputs: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
                Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL,
                NumDescs: 1, DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
                Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 { InstanceDescs: instance.as_ptr() as u64 },
                ..Default::default()
            },
            ..Default::default()
        };
        let tlas = record_on(&mut state, &dev, &list, &top, &source).unwrap();
        assert_eq!(state.structures.len(), 3);
        assert!(blas.opaque && cloned.opaque && tlas.opaque);
        let cs = dxc::compile(r#"
RaytracingAccelerationStructure scene : register(t0);
RWByteAddressBuffer output : register(u0);
[numthreads(1,1,1)]
void main(uint3 id : SV_DispatchThreadID) {
    RayDesc ray;
    ray.Origin = id.x == 0 ? float3(0.25, 0.25, 1) : float3(2, 2, 1);
    ray.Direction = float3(0, 0, -1);
    ray.TMin = 0;
    ray.TMax = 10;
    RayQuery<RAY_FLAG_FORCE_OPAQUE> q;
    q.TraceRayInline(scene, RAY_FLAG_NONE, 0xff, ray);
    while(q.Proceed()) {}
    output.Store(id.x * 4, q.CommittedStatus() == COMMITTED_TRIANGLE_HIT ? asuint(q.CommittedRayT()) : 0xffffffff);
}
"#, "main", "cs_6_6", &[]).unwrap();
        let parameters = [
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV,
                Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: 0, RegisterSpace: 0 } },
                ..Default::default()
            },
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV,
                Anonymous: D3D12_ROOT_PARAMETER_0 { Descriptor: D3D12_ROOT_DESCRIPTOR { ShaderRegister: 0, RegisterSpace: 0 } },
                ..Default::default()
            },
        ];
        let root_desc = D3D12_ROOT_SIGNATURE_DESC { NumParameters: 2, pParameters: parameters.as_ptr(), ..Default::default() };
        let mut serialized = None;
        D3D12SerializeRootSignature(&root_desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut serialized, None).unwrap();
        let serialized = serialized.unwrap();
        let root: ID3D12RootSignature = dev.CreateRootSignature(0, std::slice::from_raw_parts(serialized.GetBufferPointer().cast(), serialized.GetBufferSize())).unwrap();
        let pipeline: ID3D12PipelineState = dev.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(std::mem::transmute_copy(&root))),
            CS: D3D12_SHADER_BYTECODE { pShaderBytecode: cs.as_ptr().cast(), BytecodeLength: cs.len() },
            ..Default::default()
        }).unwrap();
        let output = allocations.buffer(&dev, 8, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS).unwrap();
        let readback = allocations.buffer(&dev, 8, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST).unwrap();
        list.SetComputeRootSignature(&root);
        list.SetPipelineState(&pipeline);
        list.SetComputeRootShaderResourceView(0, tlas.resource.GetGPUVirtualAddress());
        list.SetComputeRootUnorderedAccessView(1, output.GetGPUVirtualAddress());
        list.Dispatch(2, 1, 1);
        list.ResourceBarrier(&[transition(&output, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
        list.CopyBufferRegion(&readback, 0, &output, 0, 8);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        queue.Signal(&fence, 1).unwrap();
        crate::gpu::fence::wait(&fence, 1, &base, None, 30000).unwrap();
        let mut mapped = std::ptr::null_mut();
        readback.Map(0, None, Some(&mut mapped)).unwrap();
        let hits = std::slice::from_raw_parts(mapped.cast::<u32>(), 2);
        let console = crate::gpu::bvh::build(&[vertices], 0);
        let scene = crate::gpu::bvh::build_top(&[crate::gpu::bvh::Instance {
            transform: [[1., 0., 0., 0.], [0., 1., 0., 0.], [0., 0., 1., 0.]],
            id: 0, mask: 0xff, contribution: 0, flags: 0,
            blas_address: console_clone, blas_root: console.root, blas_bounds: console.bounds(),
        }]);
        let expected = [[0.25, 0.25, 1.], [2., 2., 1.]].map(|origin| {
            scene.trace_scene(&|address| (address == console_clone).then_some(console.data.as_slice()),
                origin, [0., 0., -1.], 10.).map_or(u32::MAX, |hit| hit.0.to_bits())
        });
        assert_eq!(expected, [1.0f32.to_bits(), u32::MAX]);
        assert_eq!(hits, expected);
        readback.Unmap(0, None);
        if query_requested() {
            *STATES.lock().unwrap() = Some(HashMap::from([(dev.as_raw() as usize, State {
                structures: state.structures.clone(), ..Default::default()
            })]));
            let bound = query_binding(&list.cast().unwrap()).unwrap();
            assert_eq!(bound.console, console_tlas);
            assert_eq!(bound.native, tlas.resource.GetGPUVirtualAddress());
            let other: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
            assert!(query_binding(&other).is_none(), "another recording must not consume an unpublished build");
            let other_dxr: ID3D12GraphicsCommandList4 = other.cast().unwrap();
            let mut unpublished = State { structures: state.structures.clone(), ..Default::default() };
            assert!(record_on(&mut unpublished, &dev, &other_dxr, &top, &source).is_err(), "TLAS cannot read another recording's unpublished BLAS");
            assert!(record_copy_on(&mut unpublished, &dev, &other_dxr, console_blas + 3 * 65536,
                console_clone, D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_CLONE.0).is_err(), "clone cannot read another recording's unpublished AS");
            assert!(unpublished.retained.is_empty());
            other.Close().unwrap();
            forget_list(list.as_raw() as usize);
            assert!(query_binding(&list.cast().unwrap()).is_none());
            *STATES.lock().unwrap() = Some(HashMap::from([(dev.as_raw() as usize, State {
                structures: state.structures.clone(), ..Default::default()
            })]));
            // Touching a byte inside a BLAS, rather than just its base, invalidates its TLAS.
            forget_range(console_blas as usize + 128, 1);
            assert!(query_binding(&list.cast().unwrap()).is_none());
            let captured = STATES.lock().unwrap().take().unwrap();
            let captured = &captured[&(dev.as_raw() as usize)];
            assert!(!captured.structures.contains_key(&console_blas));
            assert!(captured.structures.contains_key(&console_clone));
            let released = uma.alloc(2 * 65536, 65536).unwrap() as usize;
            *STATES.lock().unwrap() = Some(HashMap::from([(dev.as_raw() as usize, State {
                structures: HashMap::from([(released as u64, blas.clone()),
                    (released as u64 + 65536, tlas.clone())]), ..Default::default()
            })]));
            assert!(query_binding(&list.cast().unwrap()).is_some());
            crate::hle::kernelx::XMemFreeDefault(released as *mut c_void, 1 << 29);
            assert!(query_binding(&list.cast().unwrap()).is_none(), "XMemFree must invalidate native AS bindings");
            if let Some((released, _)) = uma.sparse_alloc(2 * 65536, 65536) {
                *STATES.lock().unwrap() = Some(HashMap::from([(dev.as_raw() as usize, State {
                    structures: HashMap::from([(released as u64, blas.clone()),
                        (released as u64 + 65536, tlas.clone())]), ..Default::default()
                })]));
                assert!(query_binding(&list.cast().unwrap()).is_some());
                assert_eq!(uma.sparse_release(released), Some(2 * 65536));
                assert!(query_binding(&list.cast().unwrap()).is_none(), "sparse release must invalidate native AS bindings");
            }
            println!("Runtime native TLAS binding and partial-range invalidation passed");
        }
        assert!(blas.resource.GetGPUVirtualAddress() != console_blas);
        assert!(state.bytes <= LIMIT);
        let mut invalid = State { structures: state.structures.clone(), ..Default::default() };
        let mut empty = bottom;
        empty.Inputs.NumDescs = 0;
        assert!(record_on(&mut invalid, &dev, &list, &empty, &source).is_err());
        assert!(!invalid.structures.contains_key(&console_tlas));
        assert!(invalid.retained.is_empty());
        *STATES.lock().unwrap() = Some(HashMap::from([(usize::MAX, State {
            structures: state.structures.clone(), ..Default::default()
        })]));
        forget(console_blas);
        let mut captured = STATES.lock().unwrap().take().unwrap();
        let captured = captured.remove(&usize::MAX).unwrap();
        assert!(!captured.structures.contains_key(&console_blas));
        assert!(!captured.structures.contains_key(&console_tlas));
        assert!(captured.structures.contains_key(&console_clone));
        let mut full = State { structures: state.structures.clone(), bytes: LIMIT, ..Default::default() };
        assert!(record_on(&mut full, &dev, &list, &bottom, &source).is_err());
        assert!(!full.structures.contains_key(&console_blas));
        assert!(!full.structures.contains_key(&console_tlas));
        assert_eq!(full.bytes, LIMIT);
        assert!(full.retained.is_empty());
        let native = native_vtable(&dev) as *const _;
        real::adopt(dev.as_raw(), real::Kind::Device);
        assert!(real::is_adopted(dev.as_raw()));
        assert_eq!(native_vtable(&dev) as *const _, native);
        let mut probe = Prepared::new(65536);
        let buffer = probe.buffer(&dev, 256, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ).unwrap();
        assert_eq!(probe.bytes, 65536);
        assert!(!real::is_adopted(buffer.as_raw()));
        assert!(probe.buffer(&dev, 256, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ).is_err());
        println!("Shadow BLAS, compact-copy and TLAS matched console BVH hit distance and miss");
    }
}
