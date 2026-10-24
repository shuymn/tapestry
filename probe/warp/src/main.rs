//! Environment probe: explicitly create a D3D12 device on the WARP adapter,
//! clear an offscreen render target (whole target, then a sub-rectangle),
//! copy it to a readback buffer, and check every pixel. Exits non-zero on any
//! failure or mismatch.

#[cfg(not(windows))]
fn main() {
    eprintln!("warp-probe runs only on Windows");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() {
    if let Err(e) = imp::run() {
        eprintln!("FAIL: {e}");
        std::process::exit(1);
    }
    println!("PASS");
}

#[cfg(windows)]
mod imp {
    use std::mem::{ManuallyDrop, size_of};

    use windows::Win32::Foundation::{CloseHandle, RECT, WAIT_OBJECT_0};
    use windows::Win32::Graphics::Direct3D::*;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    use windows::core::{Interface, Result as WinResult};

    const WIDTH: u32 = 64;
    const HEIGHT: u32 = 64;
    // Sub-rectangle cleared to the second color: x in [16, 48), y in [8, 40).
    const RECT_B: RECT = RECT {
        left: 16,
        top: 8,
        right: 48,
        bottom: 40,
    };
    // Colors as 8-bit RGBA; the clear values are k/255, which map back exactly.
    const COLOR_A: [u8; 4] = [51, 102, 153, 255];
    const COLOR_B: [u8; 4] = [255, 0, 204, 255];

    type Res<T> = std::result::Result<T, String>;

    fn ctx<T>(r: WinResult<T>, what: &str) -> Res<T> {
        r.map_err(|e| format!("{what}: {e}"))
    }

    fn to_float(c: [u8; 4]) -> [f32; 4] {
        c.map(|v| f32::from(v) / 255.0)
    }

    pub fn run() -> Res<()> {
        unsafe { run_unsafe() }
    }

    unsafe fn run_unsafe() -> Res<()> {
        let factory: IDXGIFactory4 = ctx(
            unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) },
            "CreateDXGIFactory2",
        )?;
        let adapter: IDXGIAdapter1 = ctx(unsafe { factory.EnumWarpAdapter() }, "EnumWarpAdapter")?;
        let desc = ctx(unsafe { adapter.GetDesc1() }, "GetDesc1")?;
        let name_len = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..name_len]);
        println!("adapter.name: {name}");
        println!("adapter.vendor_id: 0x{:04x}", desc.VendorId);
        println!("adapter.device_id: 0x{:04x}", desc.DeviceId);
        println!("adapter.subsys_id: 0x{:08x}", desc.SubSysId);
        println!("adapter.revision: {}", desc.Revision);
        println!("adapter.flags: 0x{:x}", desc.Flags);
        println!(
            "adapter.luid: {:08x}:{:08x}",
            desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart
        );
        match unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) } {
            Ok(v) => {
                let v = v as u64;
                println!(
                    "adapter.driver_version: {}.{}.{}.{}",
                    v >> 48,
                    (v >> 32) & 0xffff,
                    (v >> 16) & 0xffff,
                    v & 0xffff
                );
            }
            Err(e) => println!("adapter.driver_version: unavailable ({e})"),
        }
        if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) == 0 {
            return Err("EnumWarpAdapter returned an adapter without the SOFTWARE flag".into());
        }

        let mut device: Option<ID3D12Device> = None;
        ctx(
            unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device) },
            "D3D12CreateDevice(WARP)",
        )?;
        let device = device.ok_or("D3D12CreateDevice returned no device")?;

        let requested = [
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_12_0,
            D3D_FEATURE_LEVEL_12_1,
            D3D_FEATURE_LEVEL_12_2,
        ];
        let mut levels = D3D12_FEATURE_DATA_FEATURE_LEVELS {
            NumFeatureLevels: requested.len() as u32,
            pFeatureLevelsRequested: requested.as_ptr(),
            MaxSupportedFeatureLevel: D3D_FEATURE_LEVEL(0),
        };
        ctx(
            unsafe {
                device.CheckFeatureSupport(
                    D3D12_FEATURE_FEATURE_LEVELS,
                    (&raw mut levels).cast(),
                    size_of::<D3D12_FEATURE_DATA_FEATURE_LEVELS>() as u32,
                )
            },
            "CheckFeatureSupport(FEATURE_LEVELS)",
        )?;
        println!(
            "device.max_feature_level: 0x{:x}",
            levels.MaxSupportedFeatureLevel.0
        );

        let queue: ID3D12CommandQueue = ctx(
            unsafe {
                device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
            },
            "CreateCommandQueue",
        )?;
        let allocator: ID3D12CommandAllocator = ctx(
            unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) },
            "CreateCommandAllocator",
        )?;
        let list: ID3D12GraphicsCommandList = ctx(
            unsafe {
                device.CreateCommandList(
                    0,
                    D3D12_COMMAND_LIST_TYPE_DIRECT,
                    &allocator,
                    None::<&ID3D12PipelineState>,
                )
            },
            "CreateCommandList",
        )?;

        // Offscreen render target.
        let tex_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: u64::from(WIDTH),
            Height: HEIGHT,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        };
        let mut target: Option<ID3D12Resource> = None;
        ctx(
            unsafe {
                device.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &tex_desc,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    None,
                    &mut target,
                )
            },
            "CreateCommittedResource(target)",
        )?;
        let target = target.ok_or("no target resource")?;

        let rtv_heap: ID3D12DescriptorHeap = ctx(
            unsafe {
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    ..Default::default()
                })
            },
            "CreateDescriptorHeap(RTV)",
        )?;
        let rtv = unsafe { rtv_heap.GetCPUDescriptorHandleForHeapStart() };
        unsafe { device.CreateRenderTargetView(&target, None, rtv) };

        // Readback buffer sized from the copyable footprint.
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut total_bytes = 0u64;
        unsafe {
            device.GetCopyableFootprints(
                &tex_desc,
                0,
                1,
                0,
                Some(&mut footprint),
                None,
                None,
                Some(&mut total_bytes),
            )
        };
        let buf_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Alignment: 0,
            Width: total_bytes,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_UNKNOWN,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_NONE,
        };
        let mut readback: Option<ID3D12Resource> = None;
        ctx(
            unsafe {
                device.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_READBACK,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &buf_desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
            },
            "CreateCommittedResource(readback)",
        )?;
        let readback = readback.ok_or("no readback resource")?;

        // Record: clear all to A, clear the sub-rectangle to B, copy to readback.
        unsafe {
            list.ClearRenderTargetView(rtv, &to_float(COLOR_A), None);
            list.ClearRenderTargetView(rtv, &to_float(COLOR_B), Some(&[RECT_B]));
            list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                        pResource: std::mem::transmute_copy(&target),
                        Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                        StateBefore: D3D12_RESOURCE_STATE_RENDER_TARGET,
                        StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE,
                    }),
                },
            }]);
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&readback),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: footprint,
                },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&target),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        }
        ctx(unsafe { list.Close() }, "Close")?;
        let as_list: ID3D12CommandList = ctx(list.cast(), "cast command list")?;
        unsafe { queue.ExecuteCommandLists(&[Some(as_list)]) };

        let fence: ID3D12Fence = ctx(
            unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) },
            "CreateFence",
        )?;
        ctx(unsafe { queue.Signal(&fence, 1) }, "Signal")?;
        let event = ctx(
            unsafe { CreateEventW(None, false, false, None) },
            "CreateEventW",
        )?;
        ctx(
            unsafe { fence.SetEventOnCompletion(1, event) },
            "SetEventOnCompletion",
        )?;
        let wait = unsafe { WaitForSingleObject(event, INFINITE) };
        let _ = unsafe { CloseHandle(event) };
        if wait != WAIT_OBJECT_0 {
            return Err(format!("WaitForSingleObject returned {wait:?}"));
        }
        ctx(unsafe { device.GetDeviceRemovedReason() }, "device removed")?;

        // Read back and check every pixel.
        let mut ptr = std::ptr::null_mut();
        let read_range = D3D12_RANGE {
            Begin: 0,
            End: total_bytes as usize,
        };
        ctx(
            unsafe { readback.Map(0, Some(&read_range), Some(&mut ptr)) },
            "Map(readback)",
        )?;
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, total_bytes as usize) };
        let pitch = footprint.Footprint.RowPitch as usize;
        let mut mismatches = 0usize;
        let mut first = None;
        let (mut count_a, mut count_b) = (0usize, 0usize);
        for y in 0..HEIGHT as i32 {
            for x in 0..WIDTH as i32 {
                let inside = (RECT_B.left..RECT_B.right).contains(&x)
                    && (RECT_B.top..RECT_B.bottom).contains(&y);
                let expected = if inside { COLOR_B } else { COLOR_A };
                let off = y as usize * pitch + x as usize * 4;
                let got: [u8; 4] = bytes[off..off + 4].try_into().unwrap();
                if got == expected {
                    if inside {
                        count_b += 1;
                    } else {
                        count_a += 1;
                    }
                } else {
                    mismatches += 1;
                    first.get_or_insert((x, y, got, expected));
                }
            }
        }
        unsafe { readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 })) };
        println!(
            "readback: {WIDTH}x{HEIGHT} row_pitch={pitch} color_a_pixels={count_a} color_b_pixels={count_b} mismatches={mismatches}"
        );
        if let Some((x, y, got, expected)) = first {
            return Err(format!(
                "{mismatches} pixel(s) differ; first at ({x},{y}): got {got:?}, expected {expected:?}"
            ));
        }
        Ok(())
    }
}
