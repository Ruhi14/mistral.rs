use candle_core::{DType, Result, Tensor};
use candle_nn::Linear;
use float8::F8E4M3;

use super::FP8Linear;

pub struct FP8QuantizationResult {
    /// Quantized tensor (f8)
    pub qw: Tensor,
    /// Scalar, f32 tensor.
    ///
    /// Convert unquantized (bf16) to quantized tensor (fp8) as follows:
    /// `q = x * qs`
    pub quantize_scale: Tensor,
    /// Scalar, f32 tensor. Reciprocal of `quantize_scale`.
    ///
    /// Convert quantized (fp8) to unquantized (bf16) tensor as follows:
    /// `x = q * dqs`
    pub dequantize_scale: Tensor,
}

impl FP8Linear {
    pub fn quantize(data: &Tensor, dtype: DType) -> Result<FP8QuantizationResult> {
        let data = data.to_dtype(DType::BF16)?;
        let mut absmax = data.clone();
        while !absmax.dims().is_empty() {
            absmax = absmax.max(0)?;
        }
        let max_v = F8E4M3::MAX.to_f64().round();
        let scale = (max_v / absmax)?.clamp(1e-12, f64::INFINITY)?;
        let qw = data.broadcast_mul(&scale)?.to_dtype(dtype)?;
        Ok(FP8QuantizationResult {
            qw,
            quantize_scale: scale.clone().to_dtype(DType::F32)?,
            dequantize_scale: scale.recip()?.to_dtype(DType::F32)?,
        })
    }

    pub(super) fn dequantize(&self, dtype: DType) -> Result<Linear> {
        let dequant_w = self
            .lin
            .weight()
            .to_dtype(dtype)?
            .broadcast_mul(&self.dequant_w_scale.to_dtype(dtype)?)?;
        Ok(Linear::new(dequant_w, self.lin.bias().cloned()))
    }
}

#[cfg(test)]
mod tests {
    use candle_core::{DType, Device, Result, Tensor};

    use crate::fp8::FP8Linear;

    use super::FP8QuantizationResult;

    #[test]
    fn test_roundtrip_f8e4m3() -> Result<()> {
        let dev = Device::cuda_if_available(0)?;

        let data = Tensor::rand(0., 1., (32, 32), &dev)?.to_dtype(DType::F32)?;

        let FP8QuantizationResult {
            qw,
            quantize_scale: _,
            dequantize_scale,
        } = FP8Linear::quantize(&data, DType::F8E4M3)?;

        let dequant = qw.to_dtype(DType::F32)?.broadcast_mul(&dequantize_scale)?;

        let _diff = (&data - dequant)?.abs()?.mean_all()?;
        Ok(())
    }

    #[test]
    #[cfg(feature = "cuda")]
    fn test_cublaslt_matmul() -> Result<()> {
        use crate::cublaslt::{maybe_init_cublas_lt_wrapper, F8MatmulOutType, CUBLASLT_HANDLE};
        let dev = Device::new_cuda(0)?;

        let w = Tensor::rand(0., 1., (1, 16, 32), &dev)?.to_dtype(DType::F32)?;
        let mut x = Tensor::rand(0., 1., (1, 16, 32), &dev)?.to_dtype(DType::F32)?;

        // Batch matrix multiplication
        maybe_init_cublas_lt_wrapper();

        let handle = CUBLASLT_HANDLE.lock().unwrap().unwrap();

        let FP8QuantizationResult {
            qw,
            quantize_scale: quant_scale,
            dequantize_scale: dequant_a_scale,
        } = FP8Linear::quantize(&w, DType::F8E4M3)?;

        let mut dequant_b_scale = dequant_a_scale.clone();
        if !matches!(x.dtype(), DType::F8E4M3) {
            let FP8QuantizationResult {
                qw,
                quantize_scale: _,
                dequantize_scale,
            } = FP8Linear::quantize(&x, DType::F8E4M3)?;
            x = qw;
            dequant_b_scale = dequantize_scale;
        }

        let a = qw;
        let b = x;

        // FP8 quantized matmul
        let _res = handle.batch_matmul(
            &a,
            &b,
            &dequant_a_scale,
            &dequant_b_scale,
            &quant_scale,
            None,
            None,
            None,
            None,
            None,
            F8MatmulOutType::BF16,
        )?;

        Ok(())
    }
}
