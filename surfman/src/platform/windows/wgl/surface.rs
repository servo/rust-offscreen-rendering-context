// surfman/src/platform/windows/wgl/surface.rs
//
//! An implementation of the GPU device for Windows using WGL/Direct3D interoperability.

use crate::error::WindowingApiError;
use crate::renderbuffers::Renderbuffers;
use crate::{ContextID, Error, SurfaceAccess, SurfaceID, SurfaceType};
use super::context::{Context, WGL_EXTENSION_FUNCTIONS};
use super::device::Device;

use crate::gl::types::{GLenum, GLint, GLuint};
use crate::gl;
use crate::gl_utils;
use euclid::default::Size2D;
use std::fmt::{self, Debug, Formatter};
use std::marker::PhantomData;
use std::mem;
use std::os::raw::c_void;
use std::ptr;
use std::thread;
use winapi::Interface;
use winapi::shared::dxgi::IDXGIResource;
use winapi::shared::dxgiformat::DXGI_FORMAT_R8G8B8A8_UNORM;
use winapi::shared::dxgitype::DXGI_SAMPLE_DESC;
use winapi::shared::minwindef::{FALSE, UINT};
use winapi::shared::ntdef::HANDLE;
use winapi::shared::windef::HWND;
use winapi::shared::winerror;
use winapi::um::d3d11::{D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE};
use winapi::um::d3d11::{D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_TEXTURE2D_DESC};
use winapi::um::d3d11::{D3D11_USAGE_DEFAULT, ID3D11Texture2D};
use winapi::um::handleapi::INVALID_HANDLE_VALUE;
use winapi::um::wingdi;
use winapi::um::winuser;
use wio::com::ComPtr;

#[cfg(feature = "sm-winit")]
use winit::Window;
#[cfg(feature = "sm-winit")]
use winit::os::windows::WindowExt;

const SURFACE_GL_TEXTURE_TARGET: GLenum = gl::TEXTURE_2D;

const WGL_ACCESS_READ_ONLY_NV:  GLenum = 0x0000;
const WGL_ACCESS_READ_WRITE_NV: GLenum = 0x0001;

pub struct Surface {
    pub(crate) size: Size2D<i32>,
    pub(crate) context_id: ContextID,
    pub(crate) win32_objects: Win32Objects,
    pub(crate) destroyed: bool,
}

pub(crate) enum Win32Objects {
    Texture {
        d3d11_texture: ComPtr<ID3D11Texture2D>,
        dxgi_share_handle: HANDLE,
        gl_dx_interop_object: HANDLE,
        gl_texture: GLuint,
        gl_framebuffer: GLuint,
        renderbuffers: Renderbuffers,
    },
    Widget {
        window_handle: HWND,
    },
}

pub struct SurfaceTexture {
    pub(crate) surface: Surface,
    #[allow(dead_code)]
    pub(crate) local_d3d11_texture: ComPtr<ID3D11Texture2D>,
    local_gl_dx_interop_object: HANDLE,
    pub(crate) gl_texture: GLuint,
    pub(crate) phantom: PhantomData<*const ()>,
}

unsafe impl Send for Surface {}

impl Debug for Surface {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        write!(f, "Surface({:x})", self.id().0)
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        if !self.destroyed && !thread::panicking() {
            panic!("Should have destroyed the surface first with `destroy_surface()`!")
        }
    }
}

pub struct NativeWidget {
    pub(crate) window_handle: HWND,
}

impl Device {
    pub fn create_surface(&mut self,
                          context: &Context,
                          _: SurfaceAccess,
                          surface_type: &SurfaceType<NativeWidget>)
                          -> Result<Surface, Error> {
        match *surface_type {
            SurfaceType::Generic { ref size } => self.create_generic_surface(context, size),
            SurfaceType::Widget { ref native_widget } => {
                self.create_widget_surface(context, native_widget)
            }
        }
    }

    fn create_generic_surface(&mut self, context: &Context, size: &Size2D<i32>)
                              -> Result<Surface, Error> {
        let dx_interop_functions = match WGL_EXTENSION_FUNCTIONS.dx_interop_functions {
            None => return Err(Error::RequiredExtensionUnavailable),
            Some(ref dx_interop_functions) => dx_interop_functions,
        };

        unsafe {
            let _guard = self.temporarily_make_context_current(context)?;

            // Create the Direct3D 11 texture.
            let d3d11_texture2d_desc = D3D11_TEXTURE2D_DESC {
                Width: size.width as UINT,
                Height: size.height as UINT,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET,
                CPUAccessFlags: 0,
                MiscFlags: D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
            };
            let mut d3d11_texture = ptr::null_mut();
            let mut result = self.d3d11_device.CreateTexture2D(&d3d11_texture2d_desc,
                                                               ptr::null(),
                                                               &mut d3d11_texture);
            if !winerror::SUCCEEDED(result) {
                return Err(Error::SurfaceCreationFailed(WindowingApiError::Failed));
            }
            assert!(!d3d11_texture.is_null());
            let d3d11_texture = ComPtr::from_raw(d3d11_texture);

            // Upcast it to a DXGI resource.
            let mut dxgi_resource: *mut IDXGIResource = ptr::null_mut();
            result = d3d11_texture.QueryInterface(
                &IDXGIResource::uuidof(),
                &mut dxgi_resource as *mut *mut IDXGIResource as *mut *mut c_void);
            assert!(winerror::SUCCEEDED(result));
            assert!(!dxgi_resource.is_null());
            let dxgi_resource = ComPtr::from_raw(dxgi_resource);

            // Get the share handle. We'll need it both to bind to GL and to share the texture
            // across contexts.
            let mut dxgi_share_handle = INVALID_HANDLE_VALUE;
            result = dxgi_resource.GetSharedHandle(&mut dxgi_share_handle);
            assert!(winerror::SUCCEEDED(result));
            assert_ne!(dxgi_share_handle, INVALID_HANDLE_VALUE);

            // Tell GL about the share handle.
            let ok = (dx_interop_functions.DXSetResourceShareHandleNV)(
                d3d11_texture.as_raw() as *mut c_void,
                dxgi_share_handle);
            assert_ne!(ok, FALSE);

            // Make our texture object on the GL side.
            let mut gl_texture = 0;
            context.gl.GenTextures(1, &mut gl_texture);

            // Bind the GL texture to the D3D11 texture.
            let gl_dx_interop_object =
                (dx_interop_functions.DXRegisterObjectNV)(self.gl_dx_interop_device,
                                                          d3d11_texture.as_raw() as *mut c_void,
                                                          gl_texture,
                                                          gl::TEXTURE_2D,
                                                          WGL_ACCESS_READ_WRITE_NV);
            assert_ne!(gl_dx_interop_object, INVALID_HANDLE_VALUE);

            // Build our FBO.
            let mut gl_framebuffer = 0;
            context.gl.GenFramebuffers(1, &mut gl_framebuffer);
            let _guard = self.temporarily_bind_framebuffer(context, gl_framebuffer);

            // Attach the reflected D3D11 texture to that FBO.
            context.gl.FramebufferTexture2D(gl::FRAMEBUFFER,
                                            gl::COLOR_ATTACHMENT0,
                                            SURFACE_GL_TEXTURE_TARGET,
                                            gl_texture,
                                            0);

            // Create renderbuffers as appropriate, and attach them.
            let context_descriptor = self.context_descriptor(context);
            let context_attributes = self.context_descriptor_attributes(&context_descriptor);
            let renderbuffers = Renderbuffers::new(&context.gl, &size, &context_attributes);
            renderbuffers.bind_to_current_framebuffer(&context.gl);

            // FIXME(pcwalton): Do we need to acquire the keyed mutex, or does the GL driver do
            // that?

            Ok(Surface {
                size: *size,
                context_id: context.id,
                win32_objects: Win32Objects::Texture {
                    d3d11_texture,
                    dxgi_share_handle,
                    gl_dx_interop_object,
                    gl_texture,
                    gl_framebuffer,
                    renderbuffers,
                },
                destroyed: false,
            })
        }
    }

    fn create_widget_surface(&mut self, context: &Context, native_widget: &NativeWidget)
                              -> Result<Surface, Error> {
        unsafe {
            let mut widget_rect = mem::zeroed();
            let ok = winuser::GetWindowRect(native_widget.window_handle, &mut widget_rect);
            if ok == FALSE {
                return Err(Error::InvalidNativeWidget);
            }

            Ok(Surface {
                size: Size2D::new(widget_rect.right - widget_rect.left,
                                  widget_rect.bottom - widget_rect.top),
                context_id: context.id,
                win32_objects: Win32Objects::Widget {
                    window_handle: native_widget.window_handle,
                },
                destroyed: false,
            })
        }
    }

    pub fn destroy_surface(&self, context: &mut Context, mut surface: Surface)
                           -> Result<(), Error> {
        let dx_interop_functions =
            WGL_EXTENSION_FUNCTIONS.dx_interop_functions
                                   .as_ref()
                                   .expect("How did you make a surface without DX interop?");

        if context.id != surface.context_id {
            // Leak the surface, and return an error.
            surface.destroyed = true;
            return Err(Error::IncompatibleSurface);
        }

        let _guard = self.temporarily_make_context_current(context)?;

        unsafe {
            match surface.win32_objects {
                Win32Objects::Texture {
                    ref mut gl_dx_interop_object,
                    ref mut gl_texture,
                    ref mut gl_framebuffer,
                    ref mut renderbuffers,
                    d3d11_texture: _,
                    dxgi_share_handle: _,
                } => {
                    renderbuffers.destroy(&context.gl);

                    gl_utils::destroy_framebuffer(&context.gl, *gl_framebuffer);
                    *gl_framebuffer = 0;

                    context.gl.DeleteTextures(1, gl_texture);
                    *gl_texture = 0;

                    let ok = (dx_interop_functions.DXUnregisterObjectNV)(self.gl_dx_interop_device,
                                                                         *gl_dx_interop_object);
                    assert_ne!(ok, FALSE);
                    *gl_dx_interop_object = INVALID_HANDLE_VALUE;
                }
                Win32Objects::Widget { window_handle: _ } => {}
            }

            surface.destroyed = true;
        }

        Ok(())
    }

    pub fn create_surface_texture(&self, context: &mut Context, mut surface: Surface)
                                  -> Result<SurfaceTexture, Error> {
        let dxgi_share_handle = match surface.win32_objects {
            Win32Objects::Widget { .. } => {
                surface.destroyed = true;
                return Err(Error::WidgetAttached);
            }
            Win32Objects::Texture { dxgi_share_handle, .. } => dxgi_share_handle,
        };

        let dx_interop_functions =
            WGL_EXTENSION_FUNCTIONS.dx_interop_functions
                                   .as_ref()
                                   .expect("How did you make a surface without DX interop?");

        let _guard = self.temporarily_make_context_current(context)?;

        unsafe {
            // Create a new texture wrapping the shared handle.
            let mut local_d3d11_texture = ptr::null_mut();
            let result = self.d3d11_device.OpenSharedResource(dxgi_share_handle,
                                                              &ID3D11Texture2D::uuidof(),
                                                              &mut local_d3d11_texture);
            if !winerror::SUCCEEDED(result) || local_d3d11_texture.is_null() {
                surface.destroyed = true;
                return Err(Error::SurfaceImportFailed(WindowingApiError::Failed));
            }
            let local_d3d11_texture =
                ComPtr::from_raw(local_d3d11_texture as *mut ID3D11Texture2D);

            // Make GL aware of the connection between the share handle and the texture.
            let ok = (dx_interop_functions.DXSetResourceShareHandleNV)(
                local_d3d11_texture.as_raw() as *mut c_void,
                dxgi_share_handle);
            assert_ne!(ok, FALSE);

            // Create a GL texture.
            let mut gl_texture = 0;
            context.gl.GenTextures(1, &mut gl_texture);

            // Register that texture with GL/DX interop.
            let mut local_gl_dx_interop_object = (dx_interop_functions.DXRegisterObjectNV)(
                self.gl_dx_interop_device,
                local_d3d11_texture.as_raw() as *mut c_void,
                gl_texture,
                gl::TEXTURE_2D,
                WGL_ACCESS_READ_ONLY_NV);

            // Lock the texture so that we can use it.
            let ok = (dx_interop_functions.DXLockObjectsNV)(self.gl_dx_interop_device,
                                                            1,
                                                            &mut local_gl_dx_interop_object);
            assert_ne!(ok, FALSE);

            // Initialize the texture, for convenience.
            // FIXME(pcwalton): We should probably reset the bound texture after this.
            context.gl.BindTexture(gl::TEXTURE_2D, gl_texture);
            context.gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as GLint);
            context.gl.TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as GLint);
            context.gl.TexParameteri(gl::TEXTURE_2D,
                                     gl::TEXTURE_WRAP_S,
                                     gl::CLAMP_TO_EDGE as GLint);
            context.gl.TexParameteri(gl::TEXTURE_2D,
                                     gl::TEXTURE_WRAP_T,
                                     gl::CLAMP_TO_EDGE as GLint);

            // Finish up.
            Ok(SurfaceTexture {
                surface,
                local_d3d11_texture,
                local_gl_dx_interop_object,
                gl_texture,
                phantom: PhantomData,
            })
        }
    }

    pub fn destroy_surface_texture(&self,
                                   context: &mut Context,
                                   mut surface_texture: SurfaceTexture)
                                   -> Result<Surface, Error> {
        let dx_interop_functions =
            WGL_EXTENSION_FUNCTIONS.dx_interop_functions
                                   .as_ref()
                                   .expect("How did you make a surface without DX interop?");

        let _guard = self.temporarily_make_context_current(context)?;

        unsafe {
            // Unlock the texture.
            let ok = (dx_interop_functions.DXUnlockObjectsNV)(
                self.gl_dx_interop_device,
                1,
                &mut surface_texture.local_gl_dx_interop_object);
            assert_ne!(ok, FALSE);

            // Unregister the texture from GL/DX interop.
            let ok = (dx_interop_functions.DXUnregisterObjectNV)(
                self.gl_dx_interop_device,
                surface_texture.local_gl_dx_interop_object);
            assert_ne!(ok, FALSE);
            surface_texture.local_gl_dx_interop_object = INVALID_HANDLE_VALUE;

            // Destroy the GL texture.
            context.gl.DeleteTextures(1, &surface_texture.gl_texture);
            surface_texture.gl_texture = 0;
        }

        Ok(surface_texture.surface)
    }

    pub(crate) fn lock_surface(&self, surface: &Surface) {
        let mut gl_dx_interop_object = match surface.win32_objects {
            Win32Objects::Widget { .. } => return,
            Win32Objects::Texture { gl_dx_interop_object, .. } => gl_dx_interop_object,
        };

        let dx_interop_functions =
            WGL_EXTENSION_FUNCTIONS.dx_interop_functions
                                   .as_ref()
                                   .expect("How did you make a surface without DX interop?");

        unsafe {
            let ok = (dx_interop_functions.DXLockObjectsNV)(self.gl_dx_interop_device,
                                                            1,
                                                            &mut gl_dx_interop_object);
            assert_ne!(ok, FALSE);
        }
    }

    pub(crate) fn unlock_surface(&self, surface: &Surface) {
        let mut gl_dx_interop_object = match surface.win32_objects {
            Win32Objects::Widget { .. } => return,
            Win32Objects::Texture { gl_dx_interop_object, .. } => gl_dx_interop_object,
        };

        let dx_interop_functions =
            WGL_EXTENSION_FUNCTIONS.dx_interop_functions
                                   .as_ref()
                                   .expect("How did you make a surface without DX interop?");

        unsafe {
            let ok = (dx_interop_functions.DXUnlockObjectsNV)(self.gl_dx_interop_device,
                                                              1,
                                                              &mut gl_dx_interop_object);
            assert_ne!(ok, FALSE);
        }
    }

    #[inline]
    pub fn lock_surface_data<'s>(&self, surface: &'s mut Surface)
                                 -> Result<SurfaceDataGuard<'s>, Error> {
        Err(Error::Unimplemented)
    }

    #[inline]
    pub fn surface_gl_texture_target(&self) -> GLenum {
        gl::TEXTURE_2D
    }

    pub(crate) fn present_surface_without_context(&self, surface: &mut Surface)
                                                  -> Result<(), Error> {
        let window_handle = match surface.win32_objects {
            Win32Objects::Widget { window_handle } => window_handle,
            _ => return Err(Error::NoWidgetAttached),
        };

        unsafe {
            let dc = winuser::GetDC(window_handle);
            let ok = wingdi::SwapBuffers(dc);
            assert_ne!(ok, FALSE);
            winuser::ReleaseDC(window_handle, dc);
            Ok(())
        }
    }
}

impl Surface {
    #[inline]
    pub fn size(&self) -> Size2D<i32> {
        self.size
    }

    pub fn id(&self) -> SurfaceID {
        match self.win32_objects {
            Win32Objects::Texture { ref d3d11_texture, .. } => {
                SurfaceID((*d3d11_texture).as_raw() as usize)
            }
            Win32Objects::Widget { window_handle } => SurfaceID(window_handle as usize),
        }
    }

    #[inline]
    pub fn context_id(&self) -> ContextID {
        self.context_id
    }
}

impl SurfaceTexture {
    #[inline]
    pub fn gl_texture(&self) -> GLuint {
        self.gl_texture
    }
}

impl NativeWidget {
    #[cfg(feature = "sm-winit")]
    #[inline]
    pub fn from_winit_window(window: &Window) -> NativeWidget {
        NativeWidget { window_handle: window.get_hwnd() as HWND }
    }
}

pub struct SurfaceDataGuard<'a> {
    phantom: PhantomData<&'a ()>,
}