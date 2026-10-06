//! Rendering backends: the GPU "stage" (Direct3D 11 + Direct2D + DirectWrite + DirectComposition)
//! and the CPU "pill" (a tiny layered window used while the GPU stack is released).

pub mod pill;
pub mod render;
pub mod stack;
pub mod stage;
