use cosmic::cctk::cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1;
use cosmic::cctk::cosmic_protocols::workspace::v2::client::zcosmic_workspace_handle_v2;
use cosmic::cctk::wayland_client::Proxy;
use cosmic::cctk::wayland_client::protocol::wl_output;
use cosmic::cctk::wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1;
use cosmic::iced::advanced::layout::flex::Axis;
use cosmic::iced::clipboard::mime::{AllowedMimeTypes, AsMimeTypes};
use cosmic::iced::core::text::{Ellipsize, EllipsizeHeightLimit};
use cosmic::iced::core::{Shadow, window};
use cosmic::iced::platform_specific::shell::subsurface_widget::Subsurface;
use cosmic::iced::widget::{column, row};
use cosmic::iced::{self, Alignment, Border, Color, Length, Rectangle, Vector};
use cosmic::widget::{self, Widget, rectangle_tracker};
use cosmic::{Apply, Element};
use cosmic_comp_config::workspace::WorkspaceLayout;
use std::collections::HashSet;
use std::time::Instant;

use crate::backend::{self, CaptureImage};
use crate::dnd::{Drag, DragSurface, DragToplevel, DragWorkspace, DropTarget};
use crate::{App, LayerSurface, Msg, RectId, Toplevel, Workspace, anim_stagger};

struct AnimatedToplevel<'a> {
    toplevel: &'a Toplevel,
    from: Option<iced::Rectangle>,
    progress: f32,
    chrome_progress: f32,
}

fn dnd_source_with_drag_surface<D: AsMimeTypes + Send + Clone + 'static>(
    drag_content: D,
    drag_surface: DragSurface,
    id: Option<iced::id::Id>,
    child: cosmic::Element<'_, Msg>,
    drag_icon: impl Fn() -> cosmic::Element<'static, Msg> + 'static,
) -> cosmic::Element<'_, Msg> {
    let mut source = cosmic::widget::dnd_source(child)
        .drag_threshold(5.)
        .drag_content(move || drag_content.clone())
        .drag_icon(move |offset| {
            (
                drag_icon().map(|_| ()),
                cosmic::iced::core::widget::tree::State::None,
                -offset,
            )
        })
        .on_start(Some(Msg::StartDrag(drag_surface)))
        .on_finish(Some(Msg::SourceFinished))
        .on_cancel(Some(Msg::SourceFinished));
    if let Some(id) = id {
        source.set_id(id);
    }
    source.into()
}

fn dnd_destination_for_target<T>(
    target: DropTarget,
    child: cosmic::Element<'_, Msg>,
    on_finish: impl Fn(T) -> Msg + 'static,
) -> cosmic::Element<'_, Msg>
where
    T: AllowedMimeTypes,
{
    let target2 = target.clone();
    cosmic::widget::dnd_destination::dnd_destination_for_data(
        child,
        move |data: Option<T>, _action| match data {
            Some(data) => on_finish(data),
            None => Msg::Ignore,
        },
    )
    .drag_id(target.drag_id())
    .on_enter(move |actions, mime, pos| Msg::DndEnter(target.clone(), actions, mime, pos))
    .on_leave(move || Msg::DndLeave(target2.clone()))
    .into()
}

fn minimize_target(
    panel_regions: iced::Padding,
    output_size: (f32, f32),
    restore: Rectangle,
) -> Option<Rectangle> {
    const TARGET_SIZE: f32 = 32.0;
    let target_size = TARGET_SIZE.min(output_size.0).min(output_size.1);
    let (width, height) = output_size;
    let iced::Padding {
        left,
        right,
        top,
        bottom,
    } = panel_regions;
    let mut strips = Vec::new();
    if left > 0.0 {
        strips.push(Rectangle {
            x: 0.0,
            y: 0.0,
            width: left,
            height,
        });
    }
    if right > 0.0 {
        strips.push(Rectangle {
            x: width - right,
            y: 0.0,
            width: right,
            height,
        });
    }
    if top > 0.0 {
        strips.push(Rectangle {
            x: 0.0,
            y: 0.0,
            width,
            height: top,
        });
    }
    if bottom > 0.0 {
        strips.push(Rectangle {
            x: 0.0,
            y: height - bottom,
            width,
            height: bottom,
        });
    }
    let distance_to = |strip: &Rectangle| {
        let dx = (strip.x - restore.center().x)
            .max(restore.center().x - (strip.x + strip.width))
            .max(0.0);
        let dy = (strip.y - restore.center().y)
            .max(restore.center().y - (strip.y + strip.height))
            .max(0.0);
        (dx * dx + dy * dy).sqrt()
    };
    let strip = strips.iter().copied().min_by(|a, b| {
        distance_to(a)
            .partial_cmp(&distance_to(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    })?;
    let anchor_x = restore.center().x.clamp(strip.x, strip.x + strip.width);
    let anchor_y = restore.center().y.clamp(strip.y, strip.y + strip.height);
    let half = target_size / 2.0;
    Some(Rectangle {
        x: (anchor_x - half).clamp(0.0, width - target_size),
        y: (anchor_y - half).clamp(0.0, height - target_size),
        width: target_size,
        height: target_size,
    })
}

fn animated_toplevel<'a>(
    toplevel: &'a Toplevel,
    output: &wl_output::WlOutput,
    output_size: Option<(f32, f32)>,
    panel_regions: iced::Padding,
    progress: f32,
    chrome_progress: f32,
) -> AnimatedToplevel<'a> {
    let from = toplevel
        .info
        .geometry
        .get(output)
        .map(|geometry| Rectangle {
            x: geometry.x as f32,
            y: geometry.y as f32,
            width: geometry.width as f32,
            height: geometry.height as f32,
        });
    let from = if toplevel
        .info
        .state
        .contains(&zcosmic_toplevel_handle_v1::State::Minimized)
    {
        let chosen = toplevel
            .info
            .minimize_rectangle
            .get(output)
            .filter(|rect| rect.width > 0 && rect.height > 0)
            .map(|rect| Rectangle {
                x: rect.x as f32,
                y: rect.y as f32,
                width: rect.width as f32,
                height: rect.height as f32,
            })
            .or_else(|| {
                from.and_then(|restore| {
                    output_size.and_then(|size| minimize_target(panel_regions, size, restore))
                })
            });
        log::debug!(
            "minimize target for {:?}: {:?}",
            toplevel.info.title,
            chosen.map(|rect| (rect.x, rect.y, rect.width, rect.height)),
        );
        chosen
    } else {
        from
    };
    AnimatedToplevel {
        toplevel,
        from,
        progress,
        chrome_progress,
    }
}

fn pane_toplevels<'a>(
    app: &'a App,
    output: &wl_output::WlOutput,
    output_size: Option<(f32, f32)>,
    panel_regions: iced::Padding,
    matches_workspace: &dyn Fn(&backend::ExtWorkspaceHandleV1) -> bool,
) -> Vec<AnimatedToplevel<'a>> {
    let mut selected: Vec<&Toplevel> = app
        .toplevels
        .0
        .iter()
        .filter(|toplevel| {
            toplevel.info.output.contains(output)
                && toplevel
                    .info
                    .workspace
                    .iter()
                    .any(|workspace| matches_workspace(workspace))
        })
        .collect();
    selected.sort_by_key(|toplevel| toplevel.activated_at);
    selected
        .into_iter()
        .map(|toplevel| animated_toplevel(toplevel, output, output_size, panel_regions, 1.0, 1.0))
        .collect()
}

fn slide_offset(bounds: Rectangle, layout: WorkspaceLayout, fraction: f32) -> Vector {
    match layout {
        WorkspaceLayout::Vertical => Vector::new(0.0, bounds.height * fraction),
        WorkspaceLayout::Horizontal => Vector::new(bounds.width * fraction, 0.0),
    }
}

pub(crate) fn layer_surface<'a>(
    app: &'a App,
    surface: &'a LayerSurface,
    window_id: window::Id,
    rectangle_track: &rectangle_tracker::RectangleTracker<RectId>,
) -> cosmic::Element<'a, Msg> {
    let mut drag_toplevel = None;
    let mut drag_workspace = None;
    match &app.drag_surface {
        Some((DragSurface::Toplevel(handle), _)) => {
            drag_toplevel = Some(handle);
        }
        Some((DragSurface::Workspace(handle), _)) => {
            drag_workspace = Some(handle);
        }
        _ => {}
    }
    #[allow(clippy::mutable_key_type)]
    let workspaces_with_toplevels = app
        .toplevels
        .0
        .iter()
        .flat_map(|t| &t.info.workspace)
        .collect::<HashSet<_>>();
    let layout = app.conf.workspace_config.workspace_layout;
    let now = Instant::now();
    let anim = app
        .anim
        .map(|anim| anim.anchored(app.first_anim_frame.get()));
    let closing = anim.is_some_and(|anim| anim.to == 0.0);
    let sidebar_alpha = anim.map_or(1.0, |anim| {
        if closing {
            anim.chrome_fade(now)
        } else {
            anim.progress(now)
        }
    });
    // track this rectangle
    let sidebar = workspaces_sidebar(
        app.workspaces.for_output(&surface.output),
        &workspaces_with_toplevels,
        &surface.output,
        layout,
        app.drop_target.as_ref(),
        drag_workspace,
        window_id,
        rectangle_track,
        sidebar_alpha,
    );
    let panel_regions = app.panel_regions(&surface.output);
    let output_size = app
        .outputs
        .iter()
        .find(|o| o.handle == surface.output)
        .map(|o| (o.width as f32, o.height as f32));
    let content_slide = if anim.is_none() {
        app.switch_anims.get(&surface.output).map(|anim| {
            (
                anim.direction as f32,
                anim.progress(now),
                anim.from_workspace.clone(),
            )
        })
    } else {
        None
    };
    let toplevels: cosmic::Element<'_, Msg> =
        if let Some((direction, progress, from_workspace)) = content_slide {
            let active_workspace = app
                .workspaces
                .for_output(&surface.output)
                .find(|workspace| workspace.is_active())
                .map(|workspace| workspace.handle().clone());
            let outgoing = crate::widgets::slide(
                toplevel_previews(
                    pane_toplevels(
                        app,
                        &surface.output,
                        output_size,
                        panel_regions,
                        &|workspace| workspace == &from_workspace,
                    ),
                    layout,
                    drag_toplevel,
                    window_id,
                    rectangle_track,
                ),
                move |bounds| slide_offset(bounds, layout, -direction * progress),
            )
            .interactive(false);
            let incoming = crate::widgets::slide(
                toplevel_previews(
                    pane_toplevels(
                        app,
                        &surface.output,
                        output_size,
                        panel_regions,
                        &|workspace| active_workspace.as_ref() == Some(workspace),
                    ),
                    layout,
                    drag_toplevel,
                    window_id,
                    rectangle_track,
                ),
                move |bounds| slide_offset(bounds, layout, direction * (1.0 - progress)),
            )
            .interactive(false);
            cosmic::iced::widget::Stack::with_children(vec![outgoing.into(), incoming.into()])
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            let mut toplevels_on_output = app
                .toplevels
                .0
                .iter()
                .filter(|i| {
                    if !i.info.output.contains(&surface.output) {
                        return false;
                    }

                    i.info.workspace.iter().any(|workspace| {
                        app.workspaces
                            .for_handle(workspace)
                            .is_some_and(|x| x.is_active())
                    })
                })
                .collect::<Vec<_>>();
            toplevels_on_output.sort_by_key(|toplevel| toplevel.activated_at);
            let reversed_stagger = closing;
            let toplevel_count = toplevels_on_output.len();
            let animated_toplevels = toplevels_on_output
                .into_iter()
                .enumerate()
                .map(|(index, toplevel)| {
                    let progress = anim.map_or(1.0, |anim| {
                        anim.progress_with_delay(
                            now,
                            anim_stagger(index, toplevel_count, reversed_stagger),
                        )
                    });
                    let chrome_progress = if closing {
                        anim.map_or(1.0, |anim| anim.chrome_fade(now))
                    } else {
                        progress
                    };
                    animated_toplevel(
                        toplevel,
                        &surface.output,
                        output_size,
                        panel_regions,
                        progress,
                        chrome_progress,
                    )
                })
                .collect::<Vec<_>>();
            toplevel_previews(
                animated_toplevels,
                layout,
                drag_toplevel,
                window_id,
                rectangle_track,
            )
            .into()
        };
    // TODO multiple active workspaces? Not currently supported by cosmic.
    let first_active_workspace = app
        .workspaces
        .for_output(&surface.output)
        .find(|w| w.is_active());
    let toplevels = if let Some(workspace) = first_active_workspace {
        dnd_destination_for_target(
            DropTarget::OutputToplevels(workspace.handle().clone(), surface.output.clone()),
            toplevels,
            Msg::DndToplevelDrop,
        )
    } else {
        // Shouldn't happen, but no drag destination if no active workspace found for output
        cosmic::Element::from(toplevels)
    };
    let container = match layout {
        WorkspaceLayout::Vertical => widget::layer_container(
            row![sidebar, toplevels]
                .spacing(12)
                .height(Length::Fill)
                .width(Length::Fill),
        ),
        WorkspaceLayout::Horizontal => widget::layer_container(
            column![sidebar, toplevels]
                .spacing(12)
                .height(Length::Fill)
                .width(Length::Fill),
        ),
    };

    let container = widget::container(container).padding(panel_regions);

    let output = surface.output.clone();

    widget::mouse_area(container)
        .on_scroll(move |delta| Msg::OnScroll(output.clone(), delta))
        .into()
}

fn close_button(on_press: Msg, progress: f32) -> cosmic::Element<'static, Msg> {
    widget::button::custom(widget::icon::from_name("window-close-symbolic").size(16))
        .class(cosmic::theme::Button::Custom {
            active: Box::new(move |_, theme| close_button_style(theme, progress)),
            disabled: Box::new(move |theme| close_button_style(theme, progress)),
            hovered: Box::new(move |_, theme| close_button_style(theme, progress)),
            pressed: Box::new(move |_, theme| close_button_style(theme, progress)),
        })
        .on_press(on_press)
        .into()
}

fn close_button_style(theme: &cosmic::Theme, progress: f32) -> cosmic::widget::button::Style {
    let destructive = &theme.cosmic().destructive_button;
    let mut style = cosmic::widget::button::Style {
        background: Some(iced::Background::Color(destructive.base.into())),
        icon_color: Some(destructive.on.into()),
        text_color: Some(destructive.on.into()),
        border_radius: theme.cosmic().corner_radii.radius_xl.into(),
        ..cosmic::widget::button::Style::new()
    };
    if progress < 1.0 {
        style.background = style
            .background
            .map(|background| background.scale_alpha(progress));
        style.icon_color = style.icon_color.map(|color| color.scale_alpha(progress));
    }
    style
}

fn title_button_style(theme: &cosmic::Theme, progress: f32) -> cosmic::widget::button::Style {
    let mut style = cosmic::widget::button::Style::new();
    if progress < 1.0 {
        style.text_color = Some(Color::from(theme.cosmic().on_bg_color()).scale_alpha(progress));
    }
    style
}

fn pin_button_style(
    theme: &cosmic::Theme,
    is_pinned: bool,
    alpha: f32,
) -> cosmic::widget::button::Style {
    let bg_color = if is_pinned {
        theme.cosmic().accent.base.into()
    } else {
        theme.cosmic().primary(theme.transparent).base.into()
    };
    let icon_color = if is_pinned {
        theme.cosmic().accent.on.into()
    } else {
        theme.cosmic().primary(theme.transparent).on.into()
    };
    let mut style = cosmic::widget::button::Style {
        icon_color: Some(icon_color),
        background: Some(iced::Background::Color(bg_color)),
        border_radius: theme.cosmic().corner_radii.radius_m.into(),
        ..cosmic::widget::button::Style::new()
    };
    if alpha < 1.0 {
        style.icon_color = style.icon_color.map(|color| color.scale_alpha(alpha));
        style.background = style
            .background
            .map(|background| background.scale_alpha(alpha));
    }
    style
}

fn pin_button(workspace: &Workspace, alpha: f32) -> cosmic::Element<'static, Msg> {
    let is_pinned = workspace.is_pinned();
    crate::widgets::visibility_wrapper(
        widget::button::custom(
            widget::icon::from_name("pin-symbolic")
                .symbolic(true)
                .size(16),
        )
        .padding([4, 8])
        .class(cosmic::theme::Button::Custom {
            // TODO adjust state for hover, etc.
            active: Box::new(move |_, theme| pin_button_style(theme, is_pinned, alpha)),
            disabled: Box::new(move |theme| pin_button_style(theme, is_pinned, alpha)),
            hovered: Box::new(move |_, theme| pin_button_style(theme, is_pinned, alpha)),
            pressed: Box::new(move |_, theme| pin_button_style(theme, is_pinned, alpha)),
        })
        // TODO style selected correctly
        .selected(workspace.is_pinned())
        .on_press(Msg::TogglePinned(workspace.handle().clone())),
        // Show pin button only if hovered or pinned; but allocate space the same way
        // regardless
        (workspace.has_cursor || workspace.is_pinned())
            && workspace
                .info
                .cosmic_capabilities
                .contains(zcosmic_workspace_handle_v2::WorkspaceCapabilities::Pin),
    )
    .into()
}

fn workspace_item_appearance(
    theme: &cosmic::Theme,
    is_active: bool,
    hovered: bool,
    alpha: f32,
) -> cosmic::widget::button::Style {
    let cosmic = theme.cosmic();
    let mut appearance = cosmic::widget::button::Style::new();
    appearance.border_radius = cosmic
        .corner_radii
        .radius_s
        .map(|x| if x < 4.0 { x } else { x + 4.0 })
        .into();
    if is_active {
        appearance.border_width = 4.0;
        appearance.border_color = cosmic.accent.base.into();
    }
    if hovered {
        appearance.background = Some(iced::Background::Color(cosmic.button.base.into()));
    }
    if alpha < 1.0 {
        appearance.text_color = Some(Color::from(cosmic.on_bg_color()).scale_alpha(alpha));
        appearance.border_color = appearance.border_color.scale_alpha(alpha);
        appearance.background = appearance
            .background
            .map(|background| background.scale_alpha(alpha));
    }
    appearance
}

fn workspace_item(
    workspace: &Workspace,
    _output: &wl_output::WlOutput,
    layout: WorkspaceLayout,
    is_drop_target: bool,
    has_workspace_drag: bool,
    alpha: f32,
) -> cosmic::Element<'static, Msg> {
    let (mut image, image_height, image_width) = if let Some(img) = workspace.img.as_ref() {
        let is_rotated = matches!(
            img.transform,
            wl_output::Transform::_90
                | wl_output::Transform::_270
                | wl_output::Transform::Flipped90
                | wl_output::Transform::Flipped270
        );
        let (effective_width, effective_height) = if is_rotated {
            // If rotated, swap width and height
            (img.height, img.width)
        } else {
            (img.width, img.height)
        };

        if effective_width > effective_height {
            (
                // Landscape: fix height
                widget::container(capture_image(Some(img), alpha)).max_height(126.0),
                126.0,
                126.0 * effective_width as f32 / effective_height as f32,
            )
        } else {
            (
                // Portrait: fix width
                widget::container(capture_image(Some(img), alpha)).max_width(160),
                160.0 * effective_height as f32 / effective_width as f32,
                160.0,
            )
        }
    } else {
        (
            widget::container(capture_image(None, alpha))
                .max_height(126.0)
                .max_width(224.0),
            126.0,
            224.0,
        )
    };

    let workspace_footer = row![
        widget::space::horizontal().width(Length::Fixed(32.0)),
        widget::text::body(fl!("workspace", number = workspace.info.name.as_str()))
            .ellipsize(Ellipsize::Middle(EllipsizeHeightLimit::Lines(1)))
            .apply(widget::container)
            .center_x(Length::Fill)
            .class(cosmic::theme::Container::custom(move |theme| {
                cosmic::iced::widget::container::Style {
                    text_color: Some(Color::from(theme.cosmic().on_bg_color()).scale_alpha(alpha)),
                    ..Default::default()
                }
            })),
        pin_button(workspace, alpha),
    ];

    // Needed to prevent footer content getting pushed out when scaling on Vertical layout
    if layout == WorkspaceLayout::Vertical {
        image = image.height(Length::Fill);
    }
    let content = column![image, workspace_footer]
        .spacing(4)
        .align_x(Alignment::Center)
        .apply(widget::container)
        .max_height(image_height + 28.0)
        .max_width(image_width);

    let is_active = workspace.is_active() && !has_workspace_drag;
    // TODO editable name?
    let mut button = widget::button::custom(content)
        .selected(is_active)
        .class(cosmic::theme::Button::Custom {
            active: Box::new(move |_focused, theme| {
                workspace_item_appearance(theme, is_active, is_drop_target, alpha)
            }),
            disabled: Box::new(move |theme| {
                workspace_item_appearance(theme, is_active, is_drop_target, alpha)
            }),
            hovered: Box::new(move |_focused, theme| {
                workspace_item_appearance(theme, is_active, true, alpha)
            }),
            pressed: Box::new(move |_focused, theme| {
                workspace_item_appearance(theme, is_active, true, alpha)
            }),
        })
        .padding(8);
    if workspace
        .info
        .capabilities
        .contains(ext_workspace_handle_v1::WorkspaceCapabilities::Activate)
    {
        button = button.on_press(Msg::ActivateWorkspace(workspace.handle().clone()));
    }

    button.into()
}

fn workspace_drag_placeholder(
    other_workspace: &Workspace,
    other_output: &wl_output::WlOutput,
    layout: WorkspaceLayout,
) -> cosmic::Element<'static, Msg> {
    let drop_target = DropTarget::WorkspaceSidebarDragPlaceholder(
        other_workspace.handle().clone(),
        other_output.clone(),
    );
    let placeholder = widget::button::custom(
        widget::Space::new()
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .class(cosmic::theme::Button::Custom {
        active: Box::new(|_, _| unreachable!()),
        disabled: Box::new(|theme| workspace_item_appearance(theme, true, true, 1.0)),
        hovered: Box::new(|_, _| unreachable!()),
        pressed: Box::new(|_, _| unreachable!()),
    })
    .padding(8);
    let placeholder = crate::widgets::match_size(
        workspace_item(other_workspace, other_output, layout, true, true, 1.0),
        placeholder,
    );
    dnd_destination_for_target(drop_target, placeholder.into(), Msg::DndWorkspaceDrop)
}

fn workspace_sidebar_entry<'a>(
    workspace: &'a Workspace,
    output: &'a wl_output::WlOutput,
    layout: WorkspaceLayout,
    is_drop_target: bool,
    has_toplevels: bool,
    has_workspace_drag: bool,
    alpha: f32,
) -> cosmic::Element<'a, Msg> {
    /* XXX
    let mouse_interaction = if is_drop_target {
        iced::mouse::Interaction::Crosshair
    } else {
        iced::mouse::Interaction::Idle
    };
    */
    let item = workspace_item(
        workspace,
        output,
        layout,
        is_drop_target,
        has_workspace_drag,
        alpha,
    );
    let item = iced::widget::mouse_area(item)
        .on_enter(Msg::EnteredWorkspaceSidebarEntry(
            workspace.handle().clone(),
            true,
        ))
        .on_exit(Msg::EnteredWorkspaceSidebarEntry(
            workspace.handle().clone(),
            false,
        ));
    let workspace_clone = workspace.clone(); // TODO avoid clone
    let output_clone = output.clone();
    let drop_target = DropTarget::WorkspaceSidebarEntry(workspace.handle().clone(), output.clone());
    let destination =
        dnd_destination_for_target(drop_target, item.into(), |drag: Drag| match drag {
            Drag::Toplevel => Msg::DndToplevelDrop(DragToplevel {}),
            Drag::Workspace => Msg::DndWorkspaceDrop(DragWorkspace {}),
        });
    // Cosmic-comp auto-removes workspaces that aren't pinned and don't have toplevels when they
    // aren't the last workspace. So it shouldn't be possible to drag.
    if (has_toplevels || workspace.is_pinned())
        && workspace
            .info
            .cosmic_capabilities
            .contains(zcosmic_workspace_handle_v2::WorkspaceCapabilities::Move)
    {
        dnd_source_with_drag_surface(
            DragWorkspace {},
            DragSurface::Workspace(workspace.handle().clone()),
            Some(workspace.dnd_source_id.clone()),
            destination,
            move || workspace_item(&workspace_clone, &output_clone, layout, false, true, 1.0),
        )
    } else {
        destination
    }
}

#[allow(clippy::mutable_key_type)]
fn workspaces_sidebar<'a>(
    workspaces: impl Iterator<Item = &'a Workspace>,
    workspaces_with_toplevels: &HashSet<&backend::ExtWorkspaceHandleV1>,
    output: &'a wl_output::WlOutput,
    layout: WorkspaceLayout,
    drop_target: Option<&DropTarget>,
    drag_workspace: Option<&'a backend::ExtWorkspaceHandleV1>,
    window_id: window::Id,
    rectangle_track: &rectangle_tracker::RectangleTracker<RectId>,
    alpha: f32,
) -> cosmic::Element<'a, Msg> {
    let mut sidebar_entries = Vec::new();
    for workspace in workspaces {
        // XXX Need dnd source with same id for drag to work; but give it 0x0 size
        if drag_workspace == Some(workspace.handle()) {
            let workspace_clone = workspace.clone();
            let output_clone = output.clone();
            let source = dnd_source_with_drag_surface(
                DragWorkspace {},
                DragSurface::Workspace(workspace.handle().clone()),
                Some(workspace.dnd_source_id.clone()),
                widget::Space::new()
                    .width(Length::Shrink)
                    .height(Length::Shrink)
                    .into(),
                move || workspace_item(&workspace_clone, &output_clone, layout, false, true, 1.0),
            );
            sidebar_entries.push(source);
            continue;
        }

        let mut drop_target_is_workspace = false;
        let mut drop_target_is_placeholder = false;
        match drop_target {
            Some(DropTarget::WorkspaceSidebarEntry(w, o))
                if (w, o) == (workspace.handle(), output) =>
            {
                drop_target_is_workspace = true;
            }
            Some(DropTarget::WorkspaceSidebarDragPlaceholder(w, o))
                if (w, o) == (workspace.handle(), output) =>
            {
                drop_target_is_placeholder = true;
            }
            _ => {}
        }

        if drag_workspace.is_some()
            && drag_workspace != Some(workspace.handle())
            && (drop_target_is_workspace || drop_target_is_placeholder)
        {
            sidebar_entries.push(workspace_drag_placeholder(workspace, output, layout));
        }
        sidebar_entries.push(workspace_sidebar_entry(
            workspace,
            output,
            layout,
            drop_target_is_workspace && drag_workspace.is_none(),
            workspaces_with_toplevels.contains(workspace.handle()),
            drag_workspace.is_some(),
            alpha,
        ));
    }
    let (axis, width, height) = match layout {
        WorkspaceLayout::Vertical => (Axis::Vertical, Length::Shrink, Length::Fill),
        WorkspaceLayout::Horizontal => (Axis::Horizontal, Length::Fill, Length::Shrink),
    };
    let sidebar_entries_container =
        widget::container(crate::widgets::workspace_bar(sidebar_entries, axis)).padding(8.0);

    widget::container(
        rectangle_track.container(
            RectId {
                id: window_id,
                toplevel_id: None,
                widget_id: None,
                workspaces_id: None,
            },
            widget::container(sidebar_entries_container)
                .width(width)
                .height(height)
                .class(cosmic::theme::Container::custom(move |theme| {
                    let mut style = cosmic::iced::widget::container::Style {
                        text_color: Some(theme.cosmic().on_bg_color().into()),
                        icon_color: Some(theme.cosmic().on_bg_color().into()),
                        background: Some(
                            iced::Color::from(theme.cosmic().background(theme.transparent).base)
                                .into(),
                        ),
                        border: Border {
                            radius: theme
                                .cosmic()
                                .radius_s()
                                .map(|x| if x < 4.0 { x } else { x + 8.0 })
                                .into(),
                            ..Default::default()
                        },
                        shadow: Shadow::default(),
                        snap: true,
                    };
                    if alpha < 1.0 {
                        style.text_color = style.text_color.map(|c| c.scale_alpha(alpha));
                        style.icon_color = style.icon_color.map(|c| c.scale_alpha(alpha));
                        style.background = style
                            .background
                            .map(|background| background.scale_alpha(alpha));
                    }
                    style
                })),
        ),
    )
    .padding(8)
    .into()
}

fn preview_button_style(
    theme: &cosmic::Theme,
    selected: bool,
    focused: bool,
    alpha: f32,
) -> cosmic::widget::button::Style {
    let cosmic = theme.cosmic();
    let mut appearance = cosmic::widget::button::Style {
        text_color: Some(cosmic.accent_text_color().into()),
        icon_color: Some(cosmic.accent.base.into()),
        border_radius: cosmic.corner_radii.radius_s.into(),
        ..cosmic::widget::button::Style::new()
    };
    if selected || focused {
        appearance.border_width = 2.0;
        appearance.border_color = cosmic.accent.base.into();
    }
    if alpha < 1.0 {
        appearance.text_color = appearance.text_color.map(|color| color.scale_alpha(alpha));
        appearance.icon_color = appearance.icon_color.map(|color| color.scale_alpha(alpha));
        appearance.border_color = appearance.border_color.scale_alpha(alpha);
    }
    appearance
}

fn toplevel_preview(
    animated: &AnimatedToplevel<'_>,
    is_being_dragged: bool,
    window_id: window::Id,
    rectangle_track: &rectangle_tracker::RectangleTracker<RectId>,
) -> cosmic::Element<'static, Msg> {
    let toplevel = animated.toplevel;
    let progress = if is_being_dragged {
        1.0
    } else {
        animated.progress
    };
    let chrome_progress = if is_being_dragged {
        1.0
    } else {
        animated.chrome_progress
    };
    let cosmic::cosmic_theme::Spacing {
        space_xxs, space_s, ..
    } = cosmic::theme::active().cosmic().spacing;

    let label = widget::text::body(toplevel.info.title.clone())
        .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)));
    let label = if let Some(icon) = &toplevel.icon {
        row![
            widget::icon(widget::icon::from_path(icon.clone()))
                .size(24)
                .opacity(chrome_progress),
            label
        ]
        .spacing(4)
    } else {
        row![label]
    }
    .align_y(Alignment::Center);

    let selected = toplevel
        .info
        .state
        .contains(&zcosmic_toplevel_handle_v1::State::Activated);
    let title: cosmic::Element<'static, Msg> = if toplevel.info.decoration_mode
        == Some(zcosmic_toplevel_handle_v1::DecorationMode::Server)
    {
        let maximized = toplevel
            .info
            .state
            .contains(&zcosmic_toplevel_handle_v1::State::Maximized);
        let window_control = move |name: &'static str, msg: Msg| {
            widget::icon::from_name(name)
                .apply(widget::button::icon)
                .padding(8)
                .class(cosmic::theme::Button::HeaderBar)
                .selected(selected)
                .icon_size(16)
                .on_press(msg)
        };
        let controls = cosmic::widget::row::with_capacity(3)
            .push_maybe(cosmic::config::show_minimize().then(
                || -> cosmic::Element<'static, Msg> {
                    window_control(
                        "window-minimize-symbolic",
                        Msg::MinimizeToplevel(toplevel.handle.clone()),
                    )
                    .into()
                },
            ))
            .push_maybe(cosmic::config::show_maximize().then(
                || -> cosmic::Element<'static, Msg> {
                    window_control(
                        if maximized {
                            "window-restore-symbolic"
                        } else {
                            "window-maximize-symbolic"
                        },
                        Msg::ToggleMaximizeToplevel(toplevel.handle.clone()),
                    )
                    .into()
                },
            ))
            .push({
                let close: cosmic::Element<'static, Msg> = window_control(
                    "window-close-symbolic",
                    Msg::CloseToplevel(toplevel.handle.clone()),
                )
                .into();
                close
            })
            .spacing(space_xxs)
            .align_y(Alignment::Center);

        let pass_through = || {
            cosmic::theme::Container::custom(|_| cosmic::iced::widget::container::Style::default())
        };
        let bar = cosmic::iced::widget::Stack::with_children([
            widget::container(
                widget::text::heading(toplevel.info.title.clone())
                    .wrapping(cosmic::iced::core::text::Wrapping::None)
                    .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1))),
            )
            .class(pass_through())
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .align_y(Alignment::Center)
            .into(),
            widget::container(row![
                widget::space::horizontal().width(Length::Fill),
                controls
            ])
            .class(pass_through())
            .width(Length::Fill)
            .height(Length::Fill)
            .align_y(Alignment::Center)
            .into(),
        ]);
        widget::container(bar)
            .width(Length::Fill)
            .height(Length::Fixed(36.0))
            .padding([2, 8, 2, 8])
            .class(cosmic::theme::Container::custom(move |theme| {
                let mut style = <cosmic::Theme as cosmic::iced::widget::container::Catalog>::style(
                    theme,
                    &cosmic::theme::Container::HeaderBar {
                        focused: selected,
                        sharp_corners: maximized,
                        transparent: false,
                    },
                );
                if chrome_progress < 1.0 {
                    style.background = style
                        .background
                        .map(|background| background.scale_alpha(chrome_progress));
                    style.border.color = style.border.color.scale_alpha(chrome_progress);
                    style.text_color = style
                        .text_color
                        .map(|color| color.scale_alpha(chrome_progress));
                    style.icon_color = style
                        .icon_color
                        .map(|color| color.scale_alpha(chrome_progress));
                }
                style
            }))
            .into()
    } else {
        row![
            // TODO tracker for each of these buttons?
            // So that they can be blurred individually like the sidebar?
            widget::button::custom(label)
                .on_press(Msg::ActivateToplevel(toplevel.handle.clone()))
                .class(cosmic::theme::Button::Custom {
                    active: Box::new(move |_, theme| {
                        title_button_style(theme, chrome_progress)
                    }),
                    disabled: Box::new(move |theme| { title_button_style(theme, chrome_progress) }),
                    hovered: Box::new(move |_, theme| {
                        title_button_style(theme, chrome_progress)
                    }),
                    pressed: Box::new(move |_, theme| {
                        title_button_style(theme, chrome_progress)
                    }),
                })
                .padding([space_xxs, space_s])
                .apply(widget::container)
                .class(cosmic::theme::Container::custom(move |theme| {
                    let mut style = cosmic::iced::widget::container::Style {
                        background: Some(
                            iced::Color::from(theme.cosmic().background(false).component.base)
                                .into(),
                        ),
                        border: Border {
                            color: theme.cosmic().bg_divider().into(),
                            width: 1.0,
                            radius: theme.cosmic().radius_xl().into(),
                        },
                        ..Default::default()
                    };
                    if chrome_progress < 1.0 {
                        style.background = style
                            .background
                            .map(|background| background.scale_alpha(chrome_progress));
                        style.border.color = style.border.color.scale_alpha(chrome_progress);
                        style.text_color = Some(
                            Color::from(theme.cosmic().on_bg_color()).scale_alpha(chrome_progress),
                        );
                        style.icon_color = Some(
                            Color::from(theme.cosmic().on_bg_color()).scale_alpha(chrome_progress),
                        );
                    }
                    style
                }))
                .apply(widget::container)
                .width(Length::Fill),
            close_button(Msg::CloseToplevel(toplevel.handle.clone()), chrome_progress)
        ]
        .spacing(8)
        .padding([0, 0, 2, 0])
        .align_y(Alignment::Center)
        .into()
    };
    let alpha: f32 = if is_being_dragged { 0.5 } else { 1.0 };
    let image_alpha = if animated.from.is_some() {
        alpha
    } else {
        alpha.min(progress)
    };
    let content = capture_image(toplevel.img.as_ref(), image_alpha);

    let preview = widget::button::custom(if toplevel.pending_move.is_some() || is_being_dragged {
        Element::from(content)
    } else {
        Element::from(rectangle_track.container(
            RectId {
                id: window_id,
                toplevel_id: Some(toplevel.handle.id()),
                widget_id: None,
                workspaces_id: Some(toplevel.info.workspace.iter().map(|h| h.id()).collect()),
            },
            crate::widgets::fly_in(content, animated.from, progress),
        ))
    })
    .selected(selected)
    .class(cosmic::theme::Button::Custom {
        active: Box::new(move |focused, theme| {
            preview_button_style(theme, selected, focused, progress)
        }),
        disabled: Box::new(move |theme| preview_button_style(theme, selected, false, progress)),
        hovered: Box::new(move |focused, theme| {
            preview_button_style(theme, selected, focused, progress)
        }),
        pressed: Box::new(move |focused, theme| {
            preview_button_style(theme, selected, focused, progress)
        }),
    })
    .on_press(Msg::ActivateToplevel(toplevel.handle.clone()));

    widget::mouse_area(crate::widgets::size_cross_nth(
        vec![title.into(), preview.into()],
        Axis::Vertical,
        1, // Allocate width to match capture image
    ))
    .on_middle_press(Msg::CloseToplevel(toplevel.handle.clone()))
    .into()
}

fn toplevel_previews_entry<'a>(
    animated: &AnimatedToplevel<'a>,
    is_being_dragged: bool,
    window_id: window::Id,
    rectangle_track: &rectangle_tracker::RectangleTracker<RectId>,
) -> cosmic::Element<'a, Msg> {
    // Dragged window still takes up space until moved, but isn't rendered while drag surface is
    // shown.
    let preview = crate::widgets::visibility_wrapper(
        toplevel_preview(animated, is_being_dragged, window_id, rectangle_track),
        !is_being_dragged,
    );
    let dragged_toplevel = animated.toplevel.clone();
    let track = rectangle_track.clone();
    dnd_source_with_drag_surface(
        DragToplevel {},
        DragSurface::Toplevel(animated.toplevel.handle.clone()),
        None,
        preview.into(),
        move || {
            let dragged = AnimatedToplevel {
                toplevel: &dragged_toplevel,
                from: None,
                progress: 1.0,
                chrome_progress: 1.0,
            };
            toplevel_preview(&dragged, true, window_id, &track)
        },
    )
}

fn toplevel_previews<'a>(
    toplevels: Vec<AnimatedToplevel<'a>>,
    layout: WorkspaceLayout,
    drag_toplevel: Option<&'a backend::ExtForeignToplevelHandleV1>,
    window_id: window::Id,
    rectangle_track: &rectangle_tracker::RectangleTracker<RectId>,
) -> cosmic::Element<'a, Msg> {
    let (width, height) = match layout {
        WorkspaceLayout::Vertical => (Length::FillPortion(4), Length::Fill),
        WorkspaceLayout::Horizontal => (Length::Fill, Length::FillPortion(4)),
    };
    let entries = toplevels
        .iter()
        .map(|animated| {
            toplevel_previews_entry(
                animated,
                drag_toplevel == Some(&animated.toplevel.handle),
                window_id,
                rectangle_track,
            )
        })
        .collect();
    //row(entries)
    widget::mouse_area(
        widget::container(crate::widgets::toplevels(entries))
            .align_x(Alignment::Center)
            .width(width)
            .height(height)
            .padding(12),
    )
    .on_press(Msg::Close)
    .into()
}

fn capture_image(image: Option<&CaptureImage>, alpha: f32) -> cosmic::Element<'static, Msg> {
    if let Some(image) = image {
        #[cfg(feature = "no-subsurfaces")]
        {
            // TODO alpha, transform
            widget::Image::new(image.image.clone()).into()
        }
        #[cfg(not(feature = "no-subsurfaces"))]
        {
            Subsurface::new(image.wl_buffer.clone())
                .alpha(alpha)
                .transform(image.transform)
                .into()
        }
    } else {
        widget::Image::new(widget::image::Handle::from_rgba(
            1,
            1,
            vec![0, 0, 0, (255.0 * alpha).round().clamp(0.0, 255.0) as u8],
        ))
        .into()
    }
}
