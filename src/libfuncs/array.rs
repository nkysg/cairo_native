//! # Array libfuncs

// TODO: A future possible improvement would be to put the array behind a double pointer and a
//   reference counter, to avoid unnecessary clones.

use super::LibfuncHelper;
use crate::{
    error::Result,
    metadata::{
        drop_overrides::DropOverridesMeta, realloc_bindings::ReallocBindingsMeta, MetadataStorage,
    },
    types::TypeBuilder,
    utils::{BlockExt, GepIndex, ProgramRegistryExt},
};
use cairo_lang_sierra::{
    extensions::{
        array::{ArrayConcreteLibfunc, ConcreteMultiPopLibfunc},
        core::{CoreLibfunc, CoreType, CoreTypeConcrete},
        lib_func::{SignatureAndTypeConcreteLibfunc, SignatureOnlyConcreteLibfunc},
        ConcreteLibfunc,
    },
    program_registry::ProgramRegistry,
};
use melior::{
    dialect::{
        arith::{self, CmpiPredicate},
        cf,
        llvm::{self, r#type::pointer},
        ods, scf,
    },
    ir::{
        attribute::{DenseI32ArrayAttribute, IntegerAttribute},
        r#type::IntegerType,
        Block, Location, Region, Value, ValueLike,
    },
    Context,
};

/// Select and call the correct libfunc builder function from the selector.
pub fn build<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    selector: &ArrayConcreteLibfunc,
) -> Result<()> {
    match selector {
        ArrayConcreteLibfunc::New(info) => {
            build_new(context, registry, entry, location, helper, metadata, info)
        }
        ArrayConcreteLibfunc::Append(info) => {
            build_append(context, registry, entry, location, helper, metadata, info)
        }
        ArrayConcreteLibfunc::PopFront(info) => {
            build_pop_front(context, registry, entry, location, helper, metadata, info)
        }
        ArrayConcreteLibfunc::PopFrontConsume(info) => {
            build_pop_front_consume(context, registry, entry, location, helper, metadata, info)
        }
        ArrayConcreteLibfunc::Get(info) => {
            build_get(context, registry, entry, location, helper, metadata, info)
        }
        ArrayConcreteLibfunc::Slice(info) => {
            build_slice(context, registry, entry, location, helper, metadata, info)
        }
        ArrayConcreteLibfunc::Len(info) => {
            build_len(context, registry, entry, location, helper, metadata, info)
        }
        ArrayConcreteLibfunc::SnapshotPopFront(info) => {
            build_snapshot_pop_front(context, registry, entry, location, helper, metadata, info)
        }
        ArrayConcreteLibfunc::SnapshotPopBack(info) => {
            build_snapshot_pop_back(context, registry, entry, location, helper, metadata, info)
        }
        ArrayConcreteLibfunc::SpanFromTuple(info) => {
            build_span_from_tuple(context, registry, entry, location, helper, metadata, info)
        }
        ArrayConcreteLibfunc::TupleFromSpan(info) => {
            build_tuple_from_span(context, registry, entry, location, helper, metadata, info)
        }
        ArrayConcreteLibfunc::SnapshotMultiPopFront(info) => build_snapshot_multi_pop_front(
            context, registry, entry, location, helper, metadata, info,
        ),
        ArrayConcreteLibfunc::SnapshotMultiPopBack(info) => build_snapshot_multi_pop_back(
            context, registry, entry, location, helper, metadata, info,
        ),
    }
}

/// Generate MLIR operations for the `array_new` libfunc.
pub fn build_new<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    info: &SignatureOnlyConcreteLibfunc,
) -> Result<()> {
    let array_ty = registry.build_type(
        context,
        helper,
        registry,
        metadata,
        &info.branch_signatures()[0].vars[0].ty,
    )?;

    let ptr = entry
        .append_op_result(ods::llvm::mlir_zero(context, pointer(context, 0), location).into())?;

    let k0 = entry.const_int(context, location, 0, 32)?;
    let value = entry.append_op_result(llvm::undef(array_ty, location))?;
    let value = entry.insert_value(context, location, value, ptr, 0)?;
    let value = entry.insert_value(context, location, value, k0, 1)?;
    let value = entry.insert_value(context, location, value, k0, 2)?;
    let value = entry.insert_value(context, location, value, k0, 3)?;

    entry.append_operation(helper.br(0, &[value], location));
    Ok(())
}

/// Generate MLIR operations for the `array_append` libfunc.
pub fn build_append<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    info: &SignatureAndTypeConcreteLibfunc,
) -> Result<()> {
    // Algorithm:
    //   - If array_end < capacity, then append.
    //   - If array_end == capacity:
    //     - If array_start == 0: realloc, then append.
    //     - If array_start != 0: memmove, then append.

    if metadata.get::<ReallocBindingsMeta>().is_none() {
        metadata.insert(ReallocBindingsMeta::new(context, helper));
    }

    let array_ty = registry.build_type(
        context,
        helper,
        registry,
        metadata,
        &info.param_signatures()[0].ty,
    )?;

    let ptr_ty = crate::ffi::get_struct_field_type_at(&array_ty, 0);
    let len_ty = crate::ffi::get_struct_field_type_at(&array_ty, 1);

    let elem_ty = registry.get_type(&info.ty)?;
    let elem_layout = elem_ty.layout(registry)?;
    let elem_stride = elem_layout.pad_to_align().size();

    let k1 = entry.const_int(context, location, 1, 32)?;

    let elem_stride = entry.const_int(context, location, elem_stride, 64)?;

    let array_end = entry.extract_value(context, location, entry.argument(0)?.into(), len_ty, 2)?;
    let array_capacity =
        entry.extract_value(context, location, entry.argument(0)?.into(), len_ty, 3)?;

    let has_tail_space = entry.append_op_result(arith::cmpi(
        context,
        CmpiPredicate::Ult,
        array_end,
        array_capacity,
        location,
    ))?;

    let handle_block = helper.append_block(Block::new(&[]));
    let memmove_block = helper.append_block(Block::new(&[]));
    let realloc_block = helper.append_block(Block::new(&[]));
    let append_block = helper.append_block(Block::new(&[(array_ty, location)]));

    entry.append_operation(cf::cond_br(
        context,
        has_tail_space,
        append_block,
        handle_block,
        &[entry.argument(0)?.into()],
        &[],
        location,
    ));

    {
        let k0 = handle_block.const_int(context, location, 0, 32)?;
        let array_start =
            handle_block.extract_value(context, location, entry.argument(0)?.into(), len_ty, 1)?;

        let has_head_space = handle_block.append_op_result(arith::cmpi(
            context,
            CmpiPredicate::Ne,
            array_start,
            k0,
            location,
        ))?;
        handle_block.append_operation(cf::cond_br(
            context,
            has_head_space,
            memmove_block,
            realloc_block,
            &[],
            &[],
            location,
        ));
    }

    {
        let array_start =
            memmove_block.extract_value(context, location, entry.argument(0)?.into(), len_ty, 1)?;

        let start_offset = memmove_block.append_op_result(arith::extui(
            array_start,
            IntegerType::new(context, 64).into(),
            location,
        ))?;
        let start_offset =
            memmove_block.append_op_result(arith::muli(start_offset, elem_stride, location))?;

        let dst_ptr =
            memmove_block.extract_value(context, location, entry.argument(0)?.into(), ptr_ty, 0)?;
        let src_ptr = memmove_block.append_op_result(llvm::get_element_ptr_dynamic(
            context,
            dst_ptr,
            &[start_offset],
            IntegerType::new(context, 8).into(),
            llvm::r#type::pointer(context, 0),
            location,
        ))?;

        let array_len =
            memmove_block.append_op_result(arith::subi(array_end, array_start, location))?;
        let memmove_len = memmove_block.append_op_result(arith::extui(
            array_len,
            IntegerType::new(context, 64).into(),
            location,
        ))?;

        let memmove_len =
            memmove_block.append_op_result(arith::muli(memmove_len, elem_stride, location))?;
        memmove_block.append_operation(
            ods::llvm::intr_memmove(
                context,
                dst_ptr,
                src_ptr,
                memmove_len,
                IntegerAttribute::new(IntegerType::new(context, 1).into(), 0),
                location,
            )
            .into(),
        );

        let k0 = memmove_block.const_int_from_type(context, location, 0, len_ty)?;
        let value =
            memmove_block.insert_value(context, location, entry.argument(0)?.into(), k0, 1)?;
        let value = memmove_block.insert_value(context, location, value, array_len, 2)?;

        memmove_block.append_operation(cf::br(append_block, &[value], location));
    }

    {
        let k8 = realloc_block.const_int(context, location, 8, 32)?;
        let k1024 = realloc_block.const_int(context, location, 1024, 32)?;

        // Array allocation growth formula:
        //   new_len = max(8, old_len + min(1024, 2 * old_len));
        let new_capacity = realloc_block.append_op_result(arith::shli(array_end, k1, location))?;
        let new_capacity =
            realloc_block.append_op_result(arith::minui(new_capacity, k1024, location))?;
        let new_capacity =
            realloc_block.append_op_result(arith::addi(new_capacity, array_end, location))?;
        let new_capacity =
            realloc_block.append_op_result(arith::maxui(new_capacity, k8, location))?;

        let realloc_size = {
            let new_capacity = realloc_block.append_op_result(arith::extui(
                new_capacity,
                IntegerType::new(context, 64).into(),
                location,
            ))?;
            realloc_block.append_op_result(arith::muli(new_capacity, elem_stride, location))?
        };

        let ptr =
            realloc_block.extract_value(context, location, entry.argument(0)?.into(), ptr_ty, 0)?;
        let ptr = realloc_block.append_op_result(ReallocBindingsMeta::realloc(
            context,
            ptr,
            realloc_size,
            location,
        ))?;

        // No need to memmove, guaranteed by the fact that if we needed to memmove we'd have gone
        // through the memmove block instead of reallocating.

        let value =
            realloc_block.insert_value(context, location, entry.argument(0)?.into(), ptr, 0)?;
        let value = realloc_block.insert_value(context, location, value, new_capacity, 3)?;

        realloc_block.append_operation(cf::br(append_block, &[value], location));
    }

    {
        let ptr = append_block.extract_value(
            context,
            location,
            append_block.argument(0)?.into(),
            ptr_ty,
            0,
        )?;
        let array_end = append_block.extract_value(
            context,
            location,
            append_block.argument(0)?.into(),
            len_ty,
            2,
        )?;

        let offset = append_block.append_op_result(arith::extui(
            array_end,
            IntegerType::new(context, 64).into(),
            location,
        ))?;
        let offset = append_block.append_op_result(arith::muli(offset, elem_stride, location))?;
        let ptr = append_block.append_op_result(llvm::get_element_ptr_dynamic(
            context,
            ptr,
            &[offset],
            IntegerType::new(context, 8).into(),
            llvm::r#type::pointer(context, 0),
            location,
        ))?;

        append_block.store(context, location, ptr, entry.argument(1)?.into())?;

        let array_len = append_block.append_op_result(arith::addi(array_end, k1, location))?;
        let value = append_block.insert_value(
            context,
            location,
            append_block.argument(0)?.into(),
            array_len,
            2,
        )?;

        append_block.append_operation(helper.br(0, &[value], location));
    }

    Ok(())
}

/// Generate MLIR operations for the `array_len` libfunc.
pub fn build_len<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    info: &SignatureAndTypeConcreteLibfunc,
) -> Result<()> {
    let array_ty = registry.build_type(
        context,
        helper,
        registry,
        metadata,
        &info.param_signatures()[0].ty,
    )?;

    let len_ty = crate::ffi::get_struct_field_type_at(&array_ty, 1);
    let array_value = entry.argument(0)?.into();

    let array_start = entry.extract_value(context, location, array_value, len_ty, 1)?;
    let array_end = entry.extract_value(context, location, array_value, len_ty, 2)?;

    let array_len = entry.append_op_result(arith::subi(array_end, array_start, location))?;

    match metadata.get::<DropOverridesMeta>() {
        Some(drop_overrides_meta)
            if drop_overrides_meta.is_overriden(&info.signature.param_signatures[0].ty) =>
        {
            drop_overrides_meta.invoke_override(
                context,
                entry,
                location,
                &info.signature.param_signatures[0].ty,
                entry.argument(0)?.into(),
            )?;
        }
        _ => {}
    }

    entry.append_operation(helper.br(0, &[array_len], location));
    Ok(())
}

/// Generate MLIR operations for the `array_get` libfunc.
pub fn build_get<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    info: &SignatureAndTypeConcreteLibfunc,
) -> Result<()> {
    if metadata.get::<ReallocBindingsMeta>().is_none() {
        metadata.insert(ReallocBindingsMeta::new(context, helper));
    }

    let range_check =
        super::increment_builtin_counter(context, entry, location, entry.argument(0)?.into())?;

    let array_ty = registry.build_type(
        context,
        helper,
        registry,
        metadata,
        &info.param_signatures()[1].ty,
    )?;

    let elem_ty = registry.get_type(&info.ty)?;
    let elem_layout = elem_ty.layout(registry)?;
    let elem_stride = elem_layout.pad_to_align().size();
    let elem_ty = elem_ty.build(context, helper, registry, metadata, &info.ty)?;

    let ptr_ty = crate::ffi::get_struct_field_type_at(&array_ty, 0);
    let len_ty = crate::ffi::get_struct_field_type_at(&array_ty, 1);

    let value = entry.argument(1)?.into();
    let index = entry.argument(2)?.into();

    let array_start = entry.extract_value(context, location, value, len_ty, 1)?;
    let array_end = entry.extract_value(context, location, value, len_ty, 2)?;

    let array_len = entry.append_op_result(arith::subi(array_end, array_start, location))?;
    let is_valid = entry.append_op_result(arith::cmpi(
        context,
        CmpiPredicate::Ult,
        index,
        array_len,
        location,
    ))?;

    let valid_block = helper.append_block(Block::new(&[]));
    let error_block = helper.append_block(Block::new(&[]));
    entry.append_operation(cf::cond_br(
        context,
        is_valid,
        valid_block,
        error_block,
        &[],
        &[],
        location,
    ));

    {
        let ptr = valid_block.extract_value(context, location, value, ptr_ty, 0)?;

        let array_start = valid_block.append_op_result(arith::extui(
            array_start,
            IntegerType::new(context, 64).into(),
            location,
        ))?;
        let index = {
            let index = valid_block.append_op_result(arith::extui(
                index,
                IntegerType::new(context, 64).into(),
                location,
            ))?;
            valid_block.append_op_result(arith::addi(array_start, index, location))?
        };

        let elem_stride = valid_block.const_int(context, location, elem_stride, 64)?;
        let elem_offset =
            valid_block.append_op_result(arith::muli(elem_stride, index, location))?;

        let elem_ptr = valid_block.gep(
            context,
            location,
            ptr,
            &[GepIndex::Value(elem_offset)],
            IntegerType::new(context, 8).into(),
        )?;

        let elem_size = valid_block.const_int(context, location, elem_layout.size(), 64)?;

        let target_ptr = valid_block.append_op_result(
            ods::llvm::mlir_zero(context, pointer(context, 0), location).into(),
        )?;
        let target_ptr = valid_block.append_op_result(ReallocBindingsMeta::realloc(
            context, target_ptr, elem_size, location,
        ))?;
        assert_nonnull(
            context,
            valid_block,
            location,
            target_ptr,
            "realloc returned nullptr",
        )?;

        // There's no need to clone it since we're moving it out of the array.
        valid_block.memcpy(context, location, elem_ptr, target_ptr, elem_size);

        match metadata.get::<DropOverridesMeta>() {
            Some(drop_overrides_meta) if drop_overrides_meta.is_overriden(&info.ty) => {
                let array_end = valid_block.append_op_result(arith::extui(
                    array_end,
                    IntegerType::new(context, 64).into(),
                    location,
                ))?;

                let array_start = valid_block.append_op_result(arith::muli(
                    array_start,
                    elem_stride,
                    location,
                ))?;
                let array_end =
                    valid_block.append_op_result(arith::muli(array_end, elem_stride, location))?;

                valid_block.append_operation(scf::r#for(
                    array_start,
                    array_end,
                    elem_stride,
                    {
                        let region = Region::new();
                        let block = region.append_block(Block::new(&[(
                            IntegerType::new(context, 64).into(),
                            location,
                        )]));

                        let value_ptr = block.append_op_result(llvm::get_element_ptr_dynamic(
                            context,
                            ptr,
                            &[block.argument(0)?.into()],
                            IntegerType::new(context, 8).into(),
                            llvm::r#type::pointer(context, 0),
                            location,
                        ))?;

                        let is_target_element = block.append_op_result(
                            ods::llvm::icmp(
                                context,
                                IntegerType::new(context, 1).into(),
                                value_ptr,
                                elem_ptr,
                                IntegerAttribute::new(IntegerType::new(context, 64).into(), 0)
                                    .into(),
                                location,
                            )
                            .into(),
                        )?;
                        block.append_operation(scf::r#if(
                            is_target_element,
                            &[],
                            {
                                let region = Region::new();
                                let block = region.append_block(Block::new(&[]));

                                block.append_operation(scf::r#yield(&[], location));
                                region
                            },
                            {
                                let region = Region::new();
                                let block = region.append_block(Block::new(&[]));

                                let value = block.load(context, location, value_ptr, elem_ty)?;
                                drop_overrides_meta
                                    .invoke_override(context, &block, location, &info.ty, value)?;

                                block.append_operation(scf::r#yield(&[], location));
                                region
                            },
                            location,
                        ));

                        block.append_operation(scf::r#yield(&[], location));
                        region
                    },
                    location,
                ));
            }
            _ => {}
        }
        valid_block.append_operation(ReallocBindingsMeta::free(context, ptr, location));

        valid_block.append_operation(helper.br(0, &[range_check, target_ptr], location));
    }

    metadata
        .get::<DropOverridesMeta>()
        .unwrap()
        .invoke_override(
            context,
            error_block,
            location,
            &info.param_signatures()[1].ty,
            value,
        )?;
    error_block.append_operation(helper.br(1, &[range_check], location));

    Ok(())
}

/// Generate MLIR operations for the `array_pop_front` libfunc.
pub fn build_pop_front<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    info: &SignatureAndTypeConcreteLibfunc,
) -> Result<()> {
    if metadata.get::<ReallocBindingsMeta>().is_none() {
        metadata.insert(ReallocBindingsMeta::new(context, helper));
    }

    let array_ty = registry.build_type(
        context,
        helper,
        registry,
        metadata,
        &info.param_signatures()[0].ty,
    )?;

    let elem_ty = registry.get_type(&info.ty)?;
    let elem_layout = elem_ty.layout(registry)?;

    let ptr_ty = crate::ffi::get_struct_field_type_at(&array_ty, 0);
    let len_ty = crate::ffi::get_struct_field_type_at(&array_ty, 1);

    let value = entry.argument(0)?.into();

    let array_start = entry.extract_value(context, location, value, len_ty, 1)?;
    let array_end = entry.extract_value(context, location, value, len_ty, 2)?;

    let is_empty = entry.append_op_result(arith::cmpi(
        context,
        CmpiPredicate::Eq,
        array_start,
        array_end,
        location,
    ))?;

    let valid_block = helper.append_block(Block::new(&[]));
    let empty_block = helper.append_block(Block::new(&[]));
    entry.append_operation(cf::cond_br(
        context,
        is_empty,
        empty_block,
        valid_block,
        &[],
        &[],
        location,
    ));

    {
        let ptr = valid_block.extract_value(context, location, value, ptr_ty, 0)?;

        let elem_size = valid_block.const_int(context, location, elem_layout.size(), 64)?;
        let elem_offset = valid_block.append_op_result(arith::extui(
            array_start,
            IntegerType::new(context, 64).into(),
            location,
        ))?;
        let elem_offset =
            valid_block.append_op_result(arith::muli(elem_offset, elem_size, location))?;
        let ptr = valid_block.append_op_result(llvm::get_element_ptr_dynamic(
            context,
            ptr,
            &[elem_offset],
            IntegerType::new(context, 8).into(),
            llvm::r#type::pointer(context, 0),
            location,
        ))?;

        let target_ptr = valid_block.append_op_result(
            ods::llvm::mlir_zero(context, pointer(context, 0), location).into(),
        )?;
        let target_ptr = valid_block.append_op_result(ReallocBindingsMeta::realloc(
            context, target_ptr, elem_size, location,
        ))?;
        assert_nonnull(
            context,
            valid_block,
            location,
            target_ptr,
            "realloc returned nullptr",
        )?;

        valid_block.memcpy(context, location, ptr, target_ptr, elem_size);

        let k1 = valid_block.const_int(context, location, 1, 32)?;
        let new_start = valid_block.append_op_result(arith::addi(array_start, k1, location))?;
        let value = valid_block.insert_value(context, location, value, new_start, 1)?;

        valid_block.append_operation(helper.br(0, &[value, target_ptr], location));
    }

    empty_block.append_operation(helper.br(1, &[value], location));
    Ok(())
}

/// Generate MLIR operations for the `array_pop_front_consume` libfunc.
pub fn build_pop_front_consume<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    info: &SignatureAndTypeConcreteLibfunc,
) -> Result<()> {
    if metadata.get::<ReallocBindingsMeta>().is_none() {
        metadata.insert(ReallocBindingsMeta::new(context, helper));
    }

    let array_ty = registry.build_type(
        context,
        helper,
        registry,
        metadata,
        &info.param_signatures()[0].ty,
    )?;

    let elem_ty = registry.get_type(&info.ty)?;
    let elem_layout = elem_ty.layout(registry)?;

    let ptr_ty = crate::ffi::get_struct_field_type_at(&array_ty, 0);
    let len_ty = crate::ffi::get_struct_field_type_at(&array_ty, 1);

    let value = entry.argument(0)?.into();

    let array_start = entry.extract_value(context, location, value, len_ty, 1)?;
    let array_end = entry.extract_value(context, location, value, len_ty, 2)?;

    let is_empty = entry.append_op_result(arith::cmpi(
        context,
        CmpiPredicate::Eq,
        array_start,
        array_end,
        location,
    ))?;

    let valid_block = helper.append_block(Block::new(&[]));
    let empty_block = helper.append_block(Block::new(&[]));
    entry.append_operation(cf::cond_br(
        context,
        is_empty,
        empty_block,
        valid_block,
        &[],
        &[],
        location,
    ));

    {
        let ptr = valid_block.extract_value(context, location, value, ptr_ty, 0)?;

        let elem_size = valid_block.const_int(context, location, elem_layout.size(), 64)?;
        let elem_offset = valid_block.append_op_result(arith::extui(
            array_start,
            IntegerType::new(context, 64).into(),
            location,
        ))?;
        let elem_offset =
            valid_block.append_op_result(arith::muli(elem_offset, elem_size, location))?;
        let ptr = valid_block.append_op_result(llvm::get_element_ptr_dynamic(
            context,
            ptr,
            &[elem_offset],
            IntegerType::new(context, 8).into(),
            llvm::r#type::pointer(context, 0),
            location,
        ))?;

        let target_ptr = valid_block.append_op_result(
            ods::llvm::mlir_zero(context, pointer(context, 0), location).into(),
        )?;
        let target_ptr = valid_block.append_op_result(ReallocBindingsMeta::realloc(
            context, target_ptr, elem_size, location,
        ))?;
        assert_nonnull(
            context,
            valid_block,
            location,
            target_ptr,
            "realloc returned nullptr",
        )?;

        valid_block.memcpy(context, location, ptr, target_ptr, elem_size);

        let k1 = valid_block.const_int(context, location, 1, 32)?;
        let new_start = valid_block.append_op_result(arith::addi(array_start, k1, location))?;
        let value = valid_block.insert_value(context, location, value, new_start, 1)?;

        valid_block.append_operation(helper.br(0, &[value, target_ptr], location));
    }

    metadata
        .get::<DropOverridesMeta>()
        .unwrap()
        .invoke_override(
            context,
            empty_block,
            location,
            &info.param_signatures()[0].ty,
            value,
        )?;
    empty_block.append_operation(helper.br(1, &[], location));

    Ok(())
}

/// Generate MLIR operations for the `array_snapshot_pop_front` libfunc.
pub fn build_snapshot_pop_front<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    info: &SignatureAndTypeConcreteLibfunc,
) -> Result<()> {
    build_pop_front(context, registry, entry, location, helper, metadata, info)
}

/// Generate MLIR operations for the `array_snapshot_pop_back` libfunc.
pub fn build_snapshot_pop_back<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    info: &SignatureAndTypeConcreteLibfunc,
) -> Result<()> {
    if metadata.get::<ReallocBindingsMeta>().is_none() {
        metadata.insert(ReallocBindingsMeta::new(context, helper));
    }

    let array_ty = registry.build_type(
        context,
        helper,
        registry,
        metadata,
        &info.param_signatures()[0].ty,
    )?;

    let elem_ty = registry.get_type(&info.ty)?;
    let elem_layout = elem_ty.layout(registry)?;

    let ptr_ty = crate::ffi::get_struct_field_type_at(&array_ty, 0);
    let len_ty = crate::ffi::get_struct_field_type_at(&array_ty, 1);

    let value = entry.argument(0)?.into();

    let array_start = entry.extract_value(context, location, value, len_ty, 1)?;
    let array_end = entry.extract_value(context, location, value, len_ty, 2)?;
    let is_empty = entry.append_op_result(arith::cmpi(
        context,
        CmpiPredicate::Eq,
        array_start,
        array_end,
        location,
    ))?;

    let valid_block = helper.append_block(Block::new(&[]));
    let empty_block = helper.append_block(Block::new(&[]));
    entry.append_operation(cf::cond_br(
        context,
        is_empty,
        empty_block,
        valid_block,
        &[],
        &[],
        location,
    ));

    {
        let k1 = valid_block.const_int(context, location, 1, 32)?;
        let new_end = valid_block.append_op_result(arith::subi(array_end, k1, location))?;

        let ptr = valid_block.extract_value(context, location, value, ptr_ty, 0)?;

        let elem_size = valid_block.const_int(context, location, elem_layout.size(), 64)?;
        let elem_offset = valid_block.append_op_result(arith::extui(
            new_end,
            IntegerType::new(context, 64).into(),
            location,
        ))?;
        let elem_offset =
            valid_block.append_op_result(arith::muli(elem_offset, elem_size, location))?;
        let ptr = valid_block.append_op_result(llvm::get_element_ptr_dynamic(
            context,
            ptr,
            &[elem_offset],
            IntegerType::new(context, 8).into(),
            llvm::r#type::pointer(context, 0),
            location,
        ))?;

        let target_ptr = valid_block.append_op_result(
            ods::llvm::mlir_zero(context, pointer(context, 0), location).into(),
        )?;
        let target_ptr = valid_block.append_op_result(ReallocBindingsMeta::realloc(
            context, target_ptr, elem_size, location,
        ))?;
        assert_nonnull(
            context,
            valid_block,
            location,
            target_ptr,
            "realloc returned nullptr",
        )?;

        valid_block.memcpy(context, location, ptr, target_ptr, elem_size);

        let value = valid_block.insert_value(context, location, value, new_end, 2)?;

        valid_block.append_operation(helper.br(0, &[value, target_ptr], location));
    }

    empty_block.append_operation(helper.br(1, &[value], location));
    Ok(())
}

pub fn build_snapshot_multi_pop_front<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    info: &ConcreteMultiPopLibfunc,
) -> Result<()> {
    if metadata.get::<ReallocBindingsMeta>().is_none() {
        metadata.insert(ReallocBindingsMeta::new(context, helper));
    }

    let range_check = entry.argument(0)?.into();

    // Get type information

    let array_ty = registry.build_type(
        context,
        helper,
        registry,
        metadata,
        &info.param_signatures()[1].ty,
    )?;

    let popped_cty = registry.get_type(&info.popped_ty)?;
    let popped_size = popped_cty.layout(registry)?.size();
    let popped_size_value = entry.const_int(context, location, popped_size, 64)?;

    let popped_ctys = popped_cty
        .fields()
        .expect("popped type should be a tuple (ergo, has fields)");
    let popped_len = popped_ctys.len();

    let array_ptr_ty = crate::ffi::get_struct_field_type_at(&array_ty, 0);
    let array_start_ty = crate::ffi::get_struct_field_type_at(&array_ty, 1);
    let array_end_ty = crate::ffi::get_struct_field_type_at(&array_ty, 2);

    // Get array information

    let array = entry.argument(1)?.into();
    let array_ptr = entry.extract_value(context, location, array, array_ptr_ty, 0)?;
    let array_start = entry.extract_value(context, location, array, array_start_ty, 1)?;
    let array_end = entry.extract_value(context, location, array, array_end_ty, 2)?;

    // Check if operation is valid:
    // if array.end - array.start < popped_len {
    //     return
    // }

    let array_len = entry.append_op_result(arith::subi(array_end, array_start, location))?;
    let popped_len_value = entry.const_int(context, location, popped_len, 32)?;
    let is_valid = entry.append_op_result(arith::cmpi(
        context,
        CmpiPredicate::Uge,
        array_len,
        popped_len_value,
        location,
    ))?;

    let valid_block = helper.append_block(Block::new(&[]));
    let invalid_block = helper.append_block(Block::new(&[]));

    entry.append_operation(cf::cond_br(
        context,
        is_valid,
        valid_block,
        invalid_block,
        &[],
        &[],
        location,
    ));

    {
        // Get pointer to first element to pop

        let popped_ptr = {
            let single_popped_ty =
                registry.build_type(context, helper, registry, metadata, &popped_ctys[0])?;

            valid_block.append_op_result(llvm::get_element_ptr_dynamic(
                context,
                array_ptr,
                &[array_start],
                single_popped_ty,
                llvm::r#type::pointer(context, 0),
                location,
            ))?
        };

        // Allocate memory for return array

        let return_ptr = {
            let null_ptr = valid_block.append_op_result(
                ods::llvm::mlir_zero(context, pointer(context, 0), location).into(),
            )?;
            valid_block.append_op_result(ReallocBindingsMeta::realloc(
                context,
                null_ptr,
                popped_size_value,
                location,
            ))?
        };

        valid_block.memcpy(context, location, popped_ptr, return_ptr, popped_size_value);

        // Update array start (removing popped elements)

        let array = {
            let new_array_start = valid_block.append_op_result(arith::addi(
                array_start,
                popped_len_value,
                location,
            ))?;

            valid_block.insert_value(context, location, array, new_array_start, 1)?
        };

        valid_block.append_operation(helper.br(0, &[range_check, array, return_ptr], location));
    }

    invalid_block.append_operation(helper.br(1, &[range_check, array], location));

    Ok(())
}

pub fn build_snapshot_multi_pop_back<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    info: &ConcreteMultiPopLibfunc,
) -> Result<()> {
    if metadata.get::<ReallocBindingsMeta>().is_none() {
        metadata.insert(ReallocBindingsMeta::new(context, helper));
    }

    let range_check = entry.argument(0)?.into();

    // Get type information

    let array_ty = registry.build_type(
        context,
        helper,
        registry,
        metadata,
        &info.param_signatures()[1].ty,
    )?;

    let popped_cty = registry.get_type(&info.popped_ty)?;
    let popped_size = popped_cty.layout(registry)?.size();
    let popped_size_value = entry.const_int(context, location, popped_size, 64)?;

    let popped_ctys = popped_cty
        .fields()
        .expect("popped type should be a tuple (ergo, has fields)");
    let popped_len = popped_ctys.len();

    let array_ptr_ty = crate::ffi::get_struct_field_type_at(&array_ty, 0);
    let array_start_ty = crate::ffi::get_struct_field_type_at(&array_ty, 1);
    let array_end_ty = crate::ffi::get_struct_field_type_at(&array_ty, 2);

    // Get array information

    let array = entry.argument(1)?.into();
    let array_ptr = entry.extract_value(context, location, array, array_ptr_ty, 0)?;
    let array_start = entry.extract_value(context, location, array, array_start_ty, 1)?;
    let array_end = entry.extract_value(context, location, array, array_end_ty, 2)?;

    // Check if operation is valid:
    // if array.end - array.start < popped_len {
    //     return
    // }

    let array_len = entry.append_op_result(arith::subi(array_end, array_start, location))?;
    let popped_len_value = entry.const_int(context, location, popped_len, 32)?;
    let is_valid = entry.append_op_result(arith::cmpi(
        context,
        CmpiPredicate::Uge,
        array_len,
        popped_len_value,
        location,
    ))?;

    let valid_block = helper.append_block(Block::new(&[]));
    let invalid_block = helper.append_block(Block::new(&[]));

    entry.append_operation(cf::cond_br(
        context,
        is_valid,
        valid_block,
        invalid_block,
        &[],
        &[],
        location,
    ));

    {
        // Get pointer to first element to pop

        let popped_ptr = {
            let single_popped_ty =
                registry.build_type(context, helper, registry, metadata, &popped_ctys[0])?;

            let popped_start =
                valid_block.append_op_result(arith::subi(array_end, popped_len_value, location))?;

            valid_block.append_op_result(llvm::get_element_ptr_dynamic(
                context,
                array_ptr,
                &[popped_start],
                single_popped_ty,
                llvm::r#type::pointer(context, 0),
                location,
            ))?
        };

        // Allocate memory for return array

        let return_ptr = {
            let null_ptr = valid_block.append_op_result(
                ods::llvm::mlir_zero(context, pointer(context, 0), location).into(),
            )?;
            valid_block.append_op_result(ReallocBindingsMeta::realloc(
                context,
                null_ptr,
                popped_size_value,
                location,
            ))?
        };

        valid_block.memcpy(context, location, popped_ptr, return_ptr, popped_size_value);

        // Update array end (removing popped elements)

        let array = {
            let new_array_end =
                valid_block.append_op_result(arith::subi(array_end, popped_len_value, location))?;

            valid_block.insert_value(context, location, array, new_array_end, 2)?
        };

        valid_block.append_operation(helper.br(0, &[range_check, array, return_ptr], location));
    }

    invalid_block.append_operation(helper.br(1, &[range_check, array], location));

    Ok(())
}

/// Generate MLIR operations for the `array_slice` libfunc.
pub fn build_slice<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    info: &SignatureAndTypeConcreteLibfunc,
) -> Result<()> {
    let range_check =
        super::increment_builtin_counter(context, entry, location, entry.argument(0)?.into())?;

    let len_ty = IntegerType::new(context, 32).into();

    let elem_ty = registry.get_type(&info.ty)?;
    let elem_layout = elem_ty.layout(registry)?;

    let array_start =
        entry.extract_value(context, location, entry.argument(1)?.into(), len_ty, 1)?;
    let array_end = entry.extract_value(context, location, entry.argument(1)?.into(), len_ty, 2)?;

    let slice_start = entry.argument(2)?.into();
    let slice_len = entry.argument(3)?.into();
    let slice_end = entry.append_op_result(arith::addi(slice_start, slice_len, location))?;

    let slice_start = entry.append_op_result(arith::addi(array_start, slice_start, location))?;
    let slice_end = entry.append_op_result(arith::addi(array_start, slice_end, location))?;
    let lhs_bound = entry.append_op_result(arith::cmpi(
        context,
        CmpiPredicate::Uge,
        slice_start,
        array_start,
        location,
    ))?;
    let rhs_bound = entry.append_op_result(arith::cmpi(
        context,
        CmpiPredicate::Ule,
        slice_end,
        array_end,
        location,
    ))?;

    let is_valid = entry.append_op_result(arith::andi(lhs_bound, rhs_bound, location))?;

    let slice_block = helper.append_block(Block::new(&[]));
    let error_block = helper.append_block(Block::new(&[]));
    entry.append_operation(cf::cond_br(
        context,
        is_valid,
        slice_block,
        error_block,
        &[],
        &[],
        location,
    ));

    {
        let elem_ty = elem_ty.build(context, helper, registry, metadata, &info.ty)?;

        let value = entry.argument(1)?.into();
        let value = slice_block.insert_value(context, location, value, slice_start, 1)?;
        let value = slice_block.insert_value(context, location, value, slice_end, 2)?;

        let elem_stride =
            slice_block.const_int(context, location, elem_layout.pad_to_align().size(), 64)?;
        let prepare = |value| {
            let value = slice_block.append_op_result(arith::extui(
                value,
                IntegerType::new(context, 64).into(),
                location,
            ))?;
            slice_block.append_op_result(arith::muli(value, elem_stride, location))
        };

        let ptr = slice_block.extract_value(
            context,
            location,
            entry.argument(1)?.into(),
            llvm::r#type::pointer(context, 0),
            0,
        )?;
        let make_region = |drop_overrides_meta: &DropOverridesMeta| {
            let region = Region::new();
            let block = region.append_block(Block::new(&[(
                IntegerType::new(context, 64).into(),
                location,
            )]));

            let value_ptr = block.append_op_result(llvm::get_element_ptr_dynamic(
                context,
                ptr,
                &[block.argument(0)?.into()],
                IntegerType::new(context, 8).into(),
                llvm::r#type::pointer(context, 0),
                location,
            ))?;

            let value = block.load(context, location, value_ptr, elem_ty)?;
            drop_overrides_meta.invoke_override(context, &block, location, &info.ty, value)?;

            block.append_operation(scf::r#yield(&[], location));
            Result::Ok(region)
        };

        let array_start = prepare(array_start)?;
        let array_end = prepare(array_end)?;
        let slice_start = prepare(slice_start)?;
        let slice_end = prepare(slice_end)?;

        match metadata.get::<DropOverridesMeta>() {
            Some(drop_overrides_meta) if drop_overrides_meta.is_overriden(&info.ty) => {
                slice_block.append_operation(scf::r#for(
                    array_start,
                    slice_start,
                    elem_stride,
                    make_region(drop_overrides_meta)?,
                    location,
                ));
                slice_block.append_operation(scf::r#for(
                    slice_end,
                    array_end,
                    elem_stride,
                    make_region(drop_overrides_meta)?,
                    location,
                ));
            }
            _ => {}
        };

        slice_block.append_operation(helper.br(0, &[range_check, value], location));
    }

    {
        registry.build_type(
            context,
            helper,
            registry,
            metadata,
            &info.signature.param_signatures[1].ty,
        )?;

        // The following unwrap is unreachable because an array always has a drop implementation,
        // which at this point is always inserted thanks to the `build_type()` just above.
        metadata
            .get::<DropOverridesMeta>()
            .unwrap()
            .invoke_override(
                context,
                error_block,
                location,
                &info.signature.param_signatures[1].ty,
                entry.argument(1)?.into(),
            )?;

        error_block.append_operation(helper.br(1, &[range_check], location));
    }

    Ok(())
}

/// Generate MLIR operations for the `span_from_tuple` libfunc.
pub fn build_span_from_tuple<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    info: &SignatureAndTypeConcreteLibfunc,
) -> Result<()> {
    // tuple to array span (t,t,t) -> &[t,t,t]
    if metadata.get::<ReallocBindingsMeta>().is_none() {
        metadata.insert(ReallocBindingsMeta::new(context, helper));
    }

    let struct_type_info = registry.get_type(&info.ty)?;

    let struct_ty = registry.build_type(context, helper, registry, metadata, &info.ty)?;

    let container: Value = {
        // load box
        entry.load(context, location, entry.argument(0)?.into(), struct_ty)?
    };
    // TODO: Maybe reuse the pointer?
    entry.append_operation(ReallocBindingsMeta::free(
        context,
        entry.argument(0)?.into(),
        location,
    ));

    let fields = struct_type_info.fields().expect("should have fields");
    let (field_ty, field_layout) =
        registry.build_type_with_layout(context, helper, registry, metadata, &fields[0])?;
    let field_stride = field_layout.pad_to_align().size();

    let array_ty = registry.build_type(
        context,
        helper,
        registry,
        metadata,
        &info.branch_signatures()[0].vars[0].ty,
    )?;
    let len_ty = crate::ffi::get_struct_field_type_at(&array_ty, 1);

    let array_len_value = entry.const_int_from_type(context, location, fields.len(), len_ty)?;

    let array_container = entry.append_op_result(llvm::undef(array_ty, location))?;

    let k0 = entry.const_int_from_type(context, location, 0, len_ty)?;

    let array_container = entry.insert_value(context, location, array_container, k0, 1)?;
    let array_container =
        entry.insert_value(context, location, array_container, array_len_value, 2)?;
    let array_container =
        entry.insert_value(context, location, array_container, array_len_value, 3)?;

    let ptr = entry
        .append_op_result(ods::llvm::mlir_zero(context, pointer(context, 0), location).into())?;

    let field_size: Value = entry.const_int(context, location, field_stride, 64)?;
    let array_len_value_i64 =
        entry.append_op_result(arith::extui(array_len_value, field_size.r#type(), location))?;
    let total_size =
        entry.append_op_result(arith::muli(field_size, array_len_value_i64, location))?;

    let ptr = entry.append_op_result(ReallocBindingsMeta::realloc(
        context, ptr, total_size, location,
    ))?;

    for (i, _) in fields.iter().enumerate() {
        let value: Value = entry.extract_value(context, location, container, field_ty, i)?;

        let target_ptr = entry.append_op_result(llvm::get_element_ptr(
            context,
            ptr,
            DenseI32ArrayAttribute::new(context, &[i as i32]),
            field_ty,
            pointer(context, 0),
            location,
        ))?;

        entry.store(context, location, target_ptr, value)?;
    }

    let array_container = entry.insert_value(context, location, array_container, ptr, 0)?;

    entry.append_operation(helper.br(0, &[array_container], location));

    Ok(())
}

fn assert_nonnull<'ctx, 'this>(
    context: &'ctx Context,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    ptr: Value<'ctx, 'this>,
    msg: &str,
) -> Result<()> {
    let null_ptr =
        entry.append_op_result(ods::llvm::mlir_zero(context, ptr.r#type(), location).into())?;

    let ptr_is_not_null = entry.append_op_result(
        ods::llvm::icmp(
            context,
            IntegerType::new(context, 1).into(),
            ptr,
            null_ptr,
            IntegerAttribute::new(IntegerType::new(context, 64).into(), 1).into(),
            location,
        )
        .into(),
    )?;

    entry.append_operation(cf::assert(context, ptr_is_not_null, msg, location));
    Ok(())
}

pub fn build_tuple_from_span<'ctx, 'this>(
    context: &'ctx Context,
    registry: &ProgramRegistry<CoreType, CoreLibfunc>,
    entry: &'this Block<'ctx>,
    location: Location<'ctx>,
    helper: &LibfuncHelper<'ctx, 'this>,
    metadata: &mut MetadataStorage,
    info: &SignatureAndTypeConcreteLibfunc,
) -> Result<()> {
    // Tasks:
    //   - Check if sizes match.
    //   - If they do not match, jump to branch [1].
    //   - If they do match:
    //     - If start == 0 && capacity == len -> reuse the pointer
    //     - Otherwise, realloc + memcpy + free.

    if metadata.get::<ReallocBindingsMeta>().is_none() {
        metadata.insert(ReallocBindingsMeta::new(context, helper));
    }

    let array_ty = registry.get_type(&info.signature.param_signatures[0].ty)?;
    let (elem_id, elem_ty) = match array_ty {
        CoreTypeConcrete::Array(info) => (&info.ty, registry.get_type(&info.ty)?),
        CoreTypeConcrete::Snapshot(info) => match registry.get_type(&info.ty)? {
            CoreTypeConcrete::Array(info) => (&info.ty, registry.get_type(&info.ty)?),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    let elem_layout = elem_ty.layout(registry)?;

    let array_start = entry.extract_value(
        context,
        location,
        entry.argument(0)?.into(),
        IntegerType::new(context, 32).into(),
        1,
    )?;
    let array_end = entry.extract_value(
        context,
        location,
        entry.argument(0)?.into(),
        IntegerType::new(context, 32).into(),
        2,
    )?;

    let array_len = entry.append_op_result(arith::subi(array_end, array_start, location))?;
    let (tuple_len, tuple_len_val) = {
        let fields = registry.get_type(&info.ty)?.fields().unwrap();
        assert!(fields.iter().all(|f| f.id == elem_id.id));

        (
            entry.const_int(context, location, fields.len(), 32)?,
            fields.len(),
        )
    };

    let len_matches = entry.append_op_result(arith::cmpi(
        context,
        CmpiPredicate::Eq,
        array_len,
        tuple_len,
        location,
    ))?;

    let block_ok = helper.append_block(Block::new(&[]));
    let block_err = helper.append_block(Block::new(&[]));
    entry.append_operation(cf::cond_br(
        context,
        len_matches,
        block_ok,
        block_err,
        &[],
        &[],
        location,
    ));

    {
        let k0 = block_ok.const_int(context, location, 0, 32)?;
        let starts_at_zero = block_ok.append_op_result(arith::cmpi(
            context,
            CmpiPredicate::Eq,
            array_start,
            k0,
            location,
        ))?;

        let array_cap = block_ok.extract_value(
            context,
            location,
            entry.argument(0)?.into(),
            IntegerType::new(context, 32).into(),
            3,
        )?;
        let capacity_matches = block_ok.append_op_result(arith::cmpi(
            context,
            CmpiPredicate::Eq,
            array_cap,
            tuple_len,
            location,
        ))?;

        let array_ptr = block_ok.extract_value(
            context,
            location,
            entry.argument(0)?.into(),
            llvm::r#type::pointer(context, 0),
            0,
        )?;
        let should_forward_pointer =
            block_ok.append_op_result(arith::andi(starts_at_zero, capacity_matches, location))?;

        let block_clone = helper.append_block(Block::new(&[]));
        let block_forward = helper.append_block(Block::new(&[]));
        block_ok.append_operation(cf::cond_br(
            context,
            should_forward_pointer,
            block_forward,
            block_clone,
            &[],
            &[],
            location,
        ));

        {
            let elem_stride =
                block_clone.const_int(context, location, elem_layout.pad_to_align().size(), 64)?;
            let tuple_len = block_clone.append_op_result(arith::extui(
                tuple_len,
                IntegerType::new(context, 64).into(),
                location,
            ))?;
            let tuple_len =
                block_clone.append_op_result(arith::muli(tuple_len, elem_stride, location))?;

            let box_ptr = block_clone
                .append_op_result(llvm::zero(llvm::r#type::pointer(context, 0), location))?;
            let box_ptr = block_clone.append_op_result(ReallocBindingsMeta::realloc(
                context, box_ptr, tuple_len, location,
            ))?;

            let elem_offset = block_clone.append_op_result(arith::extui(
                array_start,
                IntegerType::new(context, 64).into(),
                location,
            ))?;
            let elem_offset =
                block_clone.append_op_result(arith::muli(elem_offset, elem_stride, location))?;
            let elem_ptr = block_clone.append_op_result(llvm::get_element_ptr_dynamic(
                context,
                array_ptr,
                &[elem_offset],
                IntegerType::new(context, 8).into(),
                llvm::r#type::pointer(context, 0),
                location,
            ))?;

            block_clone.append_operation(
                ods::llvm::intr_memcpy_inline(
                    context,
                    box_ptr,
                    elem_ptr,
                    IntegerAttribute::new(
                        IntegerType::new(context, 64).into(),
                        (tuple_len_val * elem_layout.pad_to_align().size()) as i64,
                    ),
                    IntegerAttribute::new(IntegerType::new(context, 1).into(), 0),
                    location,
                )
                .into(),
            );

            block_clone.append_operation(ReallocBindingsMeta::free(context, array_ptr, location));
            block_clone.append_operation(helper.br(0, &[box_ptr], location));
        }

        block_forward.append_operation(helper.br(0, &[array_ptr], location));
    }

    {
        registry.build_type(
            context,
            helper,
            registry,
            metadata,
            &info.signature.param_signatures[0].ty,
        )?;

        // The following unwrap is unreachable because an array always has a drop implementation,
        // which at this point is always inserted thanks to the `build_type()` just above.
        metadata
            .get::<DropOverridesMeta>()
            .unwrap()
            .invoke_override(
                context,
                block_err,
                location,
                &info.signature.param_signatures[0].ty,
                entry.argument(0)?.into(),
            )?;

        block_err.append_operation(helper.br(1, &[], location));
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use crate::{
        utils::{
            felt252_str,
            test::{jit_enum, jit_panic, jit_struct, load_cairo, run_program},
        },
        values::Value,
    };
    use pretty_assertions_sorted::assert_eq;
    use starknet_types_core::felt::Felt;

    #[test]
    fn run_roundtrip() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test(x: Array<u32>) -> Array<u32> {
                x
            }
        );
        let result = run_program(&program, "run_test", &[[1u32, 2u32].into()]).return_value;

        assert_eq!(result, Value::from([1u32, 2u32]));
    }

    #[test]
    fn run_append() {
        let program = load_cairo! {
            use array::ArrayTrait;

            fn run_test() -> Array<u32> {
                let mut numbers = ArrayTrait::new();
                numbers.append(4_u32);
                numbers
            }
        };
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(result, [4u32].into());
    }

    #[test]
    fn run_len() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> u32 {
                let mut numbers = ArrayTrait::new();
                numbers.append(4_u32);
                numbers.append(3_u32);
                numbers.append(2_u32);
                numbers.len()
            }
        );
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(result, 3u32.into());
    }

    #[test]
    fn run_get() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> (u32, u32, u32, u32) {
                let mut numbers = ArrayTrait::new();
                numbers.append(4_u32);
                numbers.append(3_u32);
                numbers.append(2_u32);
                numbers.append(1_u32);
                (
                    *numbers.at(0),
                    *numbers.at(1),
                    *numbers.at(2),
                    *numbers.at(3),
                )
            }
        );
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(
            result,
            jit_enum!(
                0,
                jit_struct!(jit_struct!(
                    4u32.into(),
                    3u32.into(),
                    2u32.into(),
                    1u32.into()
                ))
            )
        );
    }

    #[test]
    fn run_get_big() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> (u32, u32, u32, u32) {
                let mut numbers = ArrayTrait::new();
                numbers.append(4_u32);
                numbers.append(3_u32);
                numbers.append(2_u32);
                numbers.append(2_u32);
                numbers.append(2_u32);
                numbers.append(2_u32);
                numbers.append(2_u32);
                numbers.append(2_u32);
                numbers.append(2_u32);
                numbers.append(2_u32);
                numbers.append(2_u32);
                numbers.append(2_u32);
                numbers.append(2_u32);
                numbers.append(2_u32);
                numbers.append(2_u32);
                numbers.append(2_u32);
                numbers.append(17_u32);
                numbers.append(17_u32);
                numbers.append(18_u32);
                numbers.append(19_u32);
                numbers.append(20_u32);
                numbers.append(21_u32);
                numbers.append(22_u32);
                numbers.append(23_u32);
                (
                    *numbers.at(20),
                    *numbers.at(21),
                    *numbers.at(22),
                    *numbers.at(23),
                )
            }
        );
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(
            result,
            jit_enum!(
                0,
                jit_struct!(jit_struct!(
                    20u32.into(),
                    21u32.into(),
                    22u32.into(),
                    23u32.into()
                ))
            )
        );
    }

    #[test]
    fn run_pop_front() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> u32 {
                let mut numbers = ArrayTrait::new();
                numbers.append(4_u32);
                numbers.append(3_u32);
                let _ = numbers.pop_front();
                numbers.append(1_u32);
                *numbers.at(0)
            }
        );
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(result, jit_enum!(0, jit_struct!(3u32.into())));
    }

    #[test]
    fn run_pop_front_result() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> Option<u32> {
                let mut numbers = ArrayTrait::new();
                numbers.append(4_u32);
                numbers.append(3_u32);
                numbers.pop_front()
            }
        );
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(result, jit_enum!(0, 4u32.into()));

        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> Option<u32> {
                let mut numbers = ArrayTrait::new();
                numbers.pop_front()
            }
        );
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(result, jit_enum!(1, jit_struct!()));
    }

    #[test]
    fn run_pop_front_consume() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> u32 {
                let mut numbers = ArrayTrait::new();
                numbers.append(4_u32);
                numbers.append(3_u32);
                match numbers.pop_front_consume() {
                    Option::Some((_, x)) => x,
                    Option::None(()) => 0_u32,
                }
            }
        );
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(result, 4u32.into());
    }

    #[test]
    fn run_pop_back() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> (Option<@u32>, Option<@u32>, Option<@u32>, Option<@u32>) {
                let mut numbers = ArrayTrait::new();
                numbers.append(4_u32);
                numbers.append(3_u32);
                numbers.append(1_u32);
                let mut numbers = numbers.span();
                (
                    numbers.pop_back(),
                    numbers.pop_back(),
                    numbers.pop_back(),
                    numbers.pop_back(),
                )
            }
        );
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(
            result,
            jit_struct!(
                jit_enum!(0, 1u32.into()),
                jit_enum!(0, 3u32.into()),
                jit_enum!(0, 4u32.into()),
                jit_enum!(
                    1,
                    Value::Struct {
                        fields: Vec::new(),
                        debug_name: None,
                    }
                ),
            ),
        );
    }

    #[test]
    fn run_slice() {
        let program = load_cairo!(
            use array::Array;
            use array::ArrayTrait;
            use array::SpanTrait;
            use option::OptionTrait;
            use box::BoxTrait;

            fn run_test() -> u32 {
                let mut data: Array<u32> = ArrayTrait::new(); // Alloca (freed).
                data.append(1_u32);
                data.append(2_u32);
                data.append(3_u32);
                data.append(4_u32);
                let sp = data.span(); // Alloca (leaked).
                let slice = sp.slice(1, 2);
                data.append(5_u32);
                data.append(5_u32);
                data.append(5_u32);
                data.append(5_u32);
                data.append(5_u32); // Realloc (freed).
                data.append(5_u32);
                *slice.get(1).unwrap().unbox()
            }

        );
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(result, jit_enum!(0, jit_struct!(3u32.into())));
    }

    #[test]
    fn run_slice_fail() {
        let program = load_cairo!(
            use array::Array;
            use array::ArrayTrait;
            use array::SpanTrait;
            use option::OptionTrait;
            use box::BoxTrait;

            fn run_test() -> u32 {
                let mut data: Array<u32> = ArrayTrait::new();
                data.append(1_u32);
                data.append(2_u32);
                data.append(3_u32);
                data.append(4_u32);
                let sp = data.span();
                let slice = sp.slice(1, 4); // oob
                //data.append(5_u32);
                *slice.get(0).unwrap().unbox()
            }

        );
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(
            result,
            jit_panic!(felt252_str(
                "1637570914057682275393755530660268060279989363"
            ))
        );
    }

    #[test]
    fn run_span_from_tuple() {
        let program = load_cairo!(
            mod felt252_span_from_tuple {
                pub extern fn span_from_tuple<T>(struct_like: Box<@T>) -> @Array<felt252> nopanic;
            }

            fn run_test() -> Array<felt252> {
                let span = felt252_span_from_tuple::span_from_tuple(BoxTrait::new(@(10, 20, 30)));
                span.clone()
            }
        );
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(
            result,
            jit_enum!(
                0,
                jit_struct!(Value::from([
                    Value::Felt252(Felt::from(10)),
                    Felt::from(20).into(),
                    Felt::from(30).into()
                ]))
            )
        );
    }

    #[test]
    fn run_span_from_multi_tuple() {
        let program = load_cairo!(
            mod tuple_span_from_tuple {
                pub extern fn span_from_tuple<T>(
                    struct_like: Box<@T>
                ) -> @Array<(felt252, felt252, felt252)> nopanic;
            }

            fn run_test() {
                let multi_tuple = ((10, 20, 30), (40, 50, 60), (70, 80, 90));
                let span = tuple_span_from_tuple::span_from_tuple(BoxTrait::new(@multi_tuple));
                assert!(*span[0] == (10, 20, 30));
                assert!(*span[1] == (40, 50, 60));
                assert!(*span[2] == (70, 80, 90));
            }
        );
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(result, jit_enum!(0, jit_struct!(jit_struct!())));
    }

    #[test]
    fn seq_append1() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> Array<u32> {
                let mut data = ArrayTrait::new();
                data.append(1);
                data
            }
        );

        assert_eq!(
            run_program(&program, "run_test", &[]).return_value,
            Value::from([1u32]),
        );
    }

    #[test]
    fn seq_append2() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> Array<u32> {
                let mut data = ArrayTrait::new();
                data.append(1);
                data.append(2);
                data
            }
        );

        assert_eq!(
            run_program(&program, "run_test", &[]).return_value,
            Value::from([1u32, 2u32]),
        );
    }

    #[test]
    fn seq_append2_popf1() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> Array<u32> {
                let mut data = ArrayTrait::new();
                data.append(1);
                data.append(2);
                let _ = data.pop_front();
                data
            }
        );

        assert_eq!(
            run_program(&program, "run_test", &[]).return_value,
            Value::from([2u32]),
        );
    }

    #[test]
    fn seq_append2_popb1() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> Span<u32> {
                let mut data = ArrayTrait::new();
                data.append(1);
                data.append(2);
                let mut data = data.span();
                let _ = data.pop_back();
                data
            }
        );

        assert_eq!(
            run_program(&program, "run_test", &[]).return_value,
            jit_struct!([1u32].into())
        );
    }

    #[test]
    fn seq_append1_popf1_append1() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> Array<u32> {
                let mut data = ArrayTrait::new();
                data.append(1);
                let _ = data.pop_front();
                data.append(2);
                data
            }
        );

        assert_eq!(
            run_program(&program, "run_test", &[]).return_value,
            Value::from([2u32]),
        );
    }

    #[test]
    fn seq_append1_first() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> u32 {
                let mut data = ArrayTrait::new();
                data.append(1);
                *data.at(0)
            }
        );

        assert_eq!(
            run_program(&program, "run_test", &[]).return_value,
            Value::Enum {
                tag: 0,
                value: Box::new(Value::Struct {
                    fields: vec![Value::from(1u32)],
                    debug_name: None,
                }),
                debug_name: None,
            },
        );
    }

    #[test]
    fn seq_append2_first() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> u32 {
                let mut data = ArrayTrait::new();
                data.append(1);
                data.append(2);
                *data.at(0)
            }
        );

        assert_eq!(
            run_program(&program, "run_test", &[]).return_value,
            Value::Enum {
                tag: 0,
                value: Box::new(Value::Struct {
                    fields: vec![Value::from(1u32)],
                    debug_name: None,
                }),
                debug_name: None,
            },
        );
    }

    #[test]
    fn seq_append2_popf1_first() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> u32 {
                let mut data = ArrayTrait::new();
                data.append(1);
                data.append(2);
                let _ = data.pop_front();
                *data.at(0)
            }
        );

        assert_eq!(
            run_program(&program, "run_test", &[]).return_value,
            Value::Enum {
                tag: 0,
                value: Box::new(Value::Struct {
                    fields: vec![Value::from(2u32)],
                    debug_name: None,
                }),
                debug_name: None,
            },
        );
    }

    #[test]
    fn seq_append2_popb1_last() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> u32 {
                let mut data = ArrayTrait::new();
                data.append(1);
                data.append(2);
                let mut data_span = data.span();
                let _ = data_span.pop_back();
                let last = data_span.len() - 1;
                *data_span.at(last)
            }
        );

        assert_eq!(
            run_program(&program, "run_test", &[]).return_value,
            Value::Enum {
                tag: 0,
                value: Box::new(Value::Struct {
                    fields: vec![Value::from(1u32)],
                    debug_name: None,
                }),
                debug_name: None,
            }
        );
    }

    #[test]
    fn seq_append1_popf1_append1_first() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> u32 {
                let mut data = ArrayTrait::new();
                data.append(1);
                let _ = data.pop_front();
                data.append(2);
                *data.at(0)
            }
        );

        assert_eq!(
            run_program(&program, "run_test", &[]).return_value,
            Value::Enum {
                tag: 0,
                value: Box::new(Value::Struct {
                    fields: vec![Value::from(2u32)],
                    debug_name: None,
                }),
                debug_name: None,
            },
        );
    }

    #[test]
    fn array_clone() {
        let program = load_cairo!(
            fn run_test() -> Array<u32> {
                let x = ArrayTrait::new();
                x.clone()
            }
        );

        assert_eq!(
            run_program(&program, "run_test", &[]).return_value,
            Value::Enum {
                tag: 0,
                value: Box::new(Value::Struct {
                    fields: vec![Value::Array(vec![])],
                    debug_name: None,
                }),
                debug_name: None,
            },
        );
    }

    #[test]
    fn array_pop_back_state() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> Span<u32> {
                let mut numbers = ArrayTrait::new();
                numbers.append(1_u32);
                numbers.append(2_u32);
                numbers.append(3_u32);
                let mut numbers = numbers.span();
                let _ = numbers.pop_back();
                numbers
            }
        );

        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(result, jit_struct!([1u32, 2u32].into()));
    }

    #[test]
    fn array_empty_span() {
        // Tests snapshot_take on a empty array.
        let program = load_cairo!(
            fn run_test() -> Span<u32> {
                let x = ArrayTrait::new();
                x.span()
            }
        );

        assert_eq!(
            run_program(&program, "run_test", &[]).return_value,
            jit_struct!(Value::Array(vec![])),
        );
    }

    #[test]
    fn array_span_modify_span() {
        // Tests pop_back on a span.
        let program = load_cairo!(
            use core::array::SpanTrait;
            fn pop_elem(mut self: Span<u64>) -> Option<@u64> {
                let x = self.pop_back();
                x
            }

            fn run_test() -> Option<@u64> {
                let mut data = array![2].span();
                let x = pop_elem(data);
                x
            }
        );

        assert_eq!(
            run_program(&program, "run_test", &[]).return_value,
            jit_enum!(0, 2u64.into()),
        );
    }

    #[test]
    fn array_span_check_array() {
        // Tests pop back on a span not modifying the original array.
        let program = load_cairo!(
            use core::array::SpanTrait;
            fn pop_elem(mut self: Span<u64>) -> Option<@u64> {
                let x = self.pop_back();
                x
            }

            fn run_test() -> Array<u64> {
                let mut data = array![1, 2];
                let _x = pop_elem(data.span());
                data
            }
        );

        assert_eq!(
            run_program(&program, "run_test", &[]).return_value,
            Value::Array(vec![1u64.into(), 2u64.into()]),
        );
    }

    #[test]
    fn tuple_from_span() {
        let program = load_cairo! {
            use core::array::{tuple_from_span, FixedSizedArrayInfoImpl};

            fn run_test(x: Array<felt252>) -> [felt252; 3] {
                (*tuple_from_span::<[felt252; 3], FixedSizedArrayInfoImpl<felt252, 3>>(@x).unwrap()).unbox()
            }
        };

        assert_eq!(
            run_program(
                &program,
                "run_test",
                &[Value::Array(vec![
                    Value::Felt252(1.into()),
                    Value::Felt252(2.into()),
                    Value::Felt252(3.into()),
                ])],
            )
            .return_value,
            Value::Enum {
                tag: 0,
                value: Box::new(Value::Struct {
                    fields: vec![Value::Struct {
                        fields: vec![
                            Value::Felt252(1.into()),
                            Value::Felt252(2.into()),
                            Value::Felt252(3.into()),
                        ],
                        debug_name: None
                    }],
                    debug_name: None
                }),
                debug_name: None
            }
        );
    }

    #[test]
    fn tuple_from_span_failed() {
        let program = load_cairo! {
            use core::array::{tuple_from_span, FixedSizedArrayInfoImpl};

            fn run_test(x: Array<felt252>) -> Option<@Box<[core::felt252; 3]>> {
                tuple_from_span::<[felt252; 3], FixedSizedArrayInfoImpl<felt252, 3>>(@x)
            }
        };

        assert_eq!(
            run_program(
                &program,
                "run_test",
                &[Value::Array(vec![
                    Value::Felt252(1.into()),
                    Value::Felt252(2.into()),
                ])],
            )
            .return_value,
            jit_enum!(1, jit_struct!())
        );
    }

    #[test]
    fn snapshot_multi_pop_front() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> (Span<felt252>, @Box<[felt252; 3]>) {
                let mut numbers = array![1, 2, 3, 4, 5, 6].span();
                let popped = numbers.multi_pop_front::<3>().unwrap();

                (numbers, popped)
            }
        );
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(
            result,
            // Panic result
            jit_enum!(
                0,
                jit_struct!(
                    // Tuple
                    jit_struct!(
                        // Span of original array
                        jit_struct!(Value::Array(vec![
                            Value::Felt252(4.into()),
                            Value::Felt252(5.into()),
                            Value::Felt252(6.into()),
                        ])),
                        // Box of fixed array
                        jit_struct!(
                            Value::Felt252(1.into()),
                            Value::Felt252(2.into()),
                            Value::Felt252(3.into())
                        ),
                    )
                )
            )
        );
    }

    #[test]
    fn snapshot_failed_multi_pop_front() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> Span<felt252> {
                let mut numbers = array![1, 2].span();

                // should fail (return none)
                assert!(numbers.multi_pop_front::<3>().is_none());

                numbers
            }
        );

        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(
            result,
            // Panic result
            jit_enum!(
                0,
                jit_struct!(
                    // Span of original array
                    jit_struct!(Value::Array(vec![
                        Value::Felt252(1.into()),
                        Value::Felt252(2.into()),
                    ]),)
                )
            )
        );
    }

    #[test]
    fn snapshot_multi_pop_back() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> (Span<felt252>, @Box<[felt252; 3]>) {
                let mut numbers = array![1, 2, 3, 4, 5, 6].span();
                let popped = numbers.multi_pop_back::<3>().unwrap();

                (numbers, popped)
            }
        );
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(
            result,
            // Panic result
            jit_enum!(
                0,
                jit_struct!(
                    // Tuple
                    jit_struct!(
                        // Span of original array
                        jit_struct!(Value::Array(vec![
                            Value::Felt252(1.into()),
                            Value::Felt252(2.into()),
                            Value::Felt252(3.into()),
                        ])),
                        // Box of fixed array
                        jit_struct!(
                            Value::Felt252(4.into()),
                            Value::Felt252(5.into()),
                            Value::Felt252(6.into())
                        ),
                    )
                )
            )
        );
    }

    #[test]
    fn snapshot_failed_multi_pop_back() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> Span<felt252> {
                let mut numbers = array![1, 2].span();

                // should fail (return none)
                assert!(numbers.multi_pop_back::<3>().is_none());

                numbers
            }
        );

        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(
            result,
            // Panic result
            jit_enum!(
                0,
                jit_struct!(
                    // Span of original array
                    jit_struct!(Value::Array(vec![
                        Value::Felt252(1.into()),
                        Value::Felt252(2.into()),
                    ]),)
                )
            )
        );
    }

    #[test]
    fn snapshot_multi_pop_back_front() {
        let program = load_cairo!(
            use array::ArrayTrait;

            fn run_test() -> (Span<felt252>, @Box<[felt252; 2]>, @Box<[felt252; 2]>) {
                let mut numbers = array![1, 2, 3, 4, 5, 6].span();
                let popped_front = numbers.multi_pop_front::<2>().unwrap();
                let popped_back = numbers.multi_pop_back::<2>().unwrap();

                (numbers, popped_front, popped_back)
            }
        );
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(
            result,
            // Panic result
            jit_enum!(
                0,
                jit_struct!(
                    // Tuple
                    jit_struct!(
                        // Span of original array
                        jit_struct!(Value::Array(vec![
                            Value::Felt252(3.into()),
                            Value::Felt252(4.into()),
                        ])),
                        // Box of fixed array
                        jit_struct!(Value::Felt252(1.into()), Value::Felt252(2.into()),),
                        // Box of fixed array
                        jit_struct!(Value::Felt252(5.into()), Value::Felt252(6.into())),
                    )
                )
            )
        );
    }

    /// Test to ensure that the returned element in `array_get` does NOT get dropped.
    #[test]
    fn array_get_avoid_dropping_element() {
        let program = load_cairo! {
            use core::{array::{array_append, array_at, array_new}, box::{into_box, unbox}};

            fn run_test() -> @Box<felt252> {
                let mut x: Array<Box<felt252>> = array_new();
                array_append(ref x, into_box(42));

                unbox(array_at(@x, 0))
            }
        };
        let result = run_program(&program, "run_test", &[]).return_value;

        assert_eq!(result, jit_enum!(0, jit_struct!(Value::Felt252(42.into()))));
    }
}
