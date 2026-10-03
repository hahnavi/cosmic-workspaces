// Widget that animates its (leaf) content between a starting rectangle and the
// rectangle assigned by layout.
//
// The content is drawn with a reconstructed layout each frame, which works for
// widgets that position themselves from `layout.bounds()`, like the subsurface
// used for capture images. Input handling and layout stay at the final position.

use cosmic::iced::advanced::widget::{Operation, Tree};
use cosmic::iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use cosmic::iced::event::Event;
use cosmic::iced::{Length, Point, Rectangle, Size, Vector};
use std::marker::PhantomData;

fn lerp_rect(from: Rectangle, to: Rectangle, t: f32) -> Rectangle {
    Rectangle {
        x: from.x + (to.x - from.x) * t,
        y: from.y + (to.y - from.y) * t,
        width: from.width + (to.width - from.width) * t,
        height: from.height + (to.height - from.height) * t,
    }
}

/// Wrap `content` so that it is drawn interpolating from `from` to its laid out
/// bounds. `progress` is the eased animation progress, where 0.0 draws at
/// `from` and 1.0 draws at the laid out position. When `from` is `None` or
/// progress is 1.0, the content is drawn normally.
pub fn fly_in<'a, Msg, T: Into<cosmic::Element<'a, Msg>>>(
    inner: T,
    from: Option<Rectangle>,
    progress: f32,
) -> FlyIn<'a, Msg> {
    FlyIn {
        content: inner.into(),
        from,
        progress: progress.clamp(0.0, 1.0),
        _msg: PhantomData,
    }
}

pub struct FlyIn<'a, Msg> {
    content: cosmic::Element<'a, Msg>,
    from: Option<Rectangle>,
    progress: f32,
    _msg: PhantomData<Msg>,
}

impl<Msg> Widget<Msg, cosmic::Theme, cosmic::Renderer> for FlyIn<'_, Msg> {
    delegate::delegate! {
        to self.content.as_widget() {
            fn size(&self) -> Size<Length>;
            fn size_hint(&self) -> Size<Length>;
            fn mouse_interaction(
                &self,
                _tree: &Tree,
                _layout: Layout<'_>,
                _cursor: mouse::Cursor,
                _viewport: &Rectangle,
                _renderer: &cosmic::Renderer,
            ) -> mouse::Interaction;
        }
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation<()>,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Msg>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let animating = self.from.is_some_and(|from| {
            self.progress < 1.0
                && from.width > 0.0
                && from.height > 0.0
                && layout.bounds().width > 0.0
                && layout.bounds().height > 0.0
        });
        if !animating {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                layout,
                cursor,
                viewport,
            );
            return;
        }

        let target = lerp_rect(self.from.unwrap(), layout.bounds(), self.progress);
        if target.width < 1.0 || target.height < 1.0 {
            return;
        }
        // The wrapped content is a leaf widget that positions itself from
        // `layout.bounds()`, so it can be drawn at any rectangle by
        // reconstructing the layout. Parent widgets clip the viewport to the
        // laid out bounds, so expand it to cover the animated position.
        let node = layout::Node::new(target.size()).move_to(target.position());
        let viewport = viewport.union(&target);
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            Layout::new(&node),
            cursor,
            &viewport,
        );
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut [&mut self.content]);
    }
}

impl<'a, Msg: 'a> From<FlyIn<'a, Msg>> for cosmic::Element<'a, Msg> {
    fn from(widget: FlyIn<'a, Msg>) -> Self {
        cosmic::Element::new(widget)
    }
}

/// Widget that draws (and hit-tests) its content displaced by an offset
/// computed from the content's natural bounds.
///
/// Unlike `fly_in`, this works for whole subtrees, not just leaf widgets:
/// displacing the content's root layout node shifts every descendant with it
/// (descendant layout positions are relative to it), including capture
/// subsurfaces, and input tracking follows along.
pub fn slide<'a, Msg, T: Into<cosmic::Element<'a, Msg>>>(
    inner: T,
    offset: impl Fn(Rectangle) -> Vector + 'a,
) -> Slide<'a, Msg> {
    Slide {
        content: inner.into(),
        offset: Box::new(offset),
        interactive: true,
        _msg: PhantomData,
    }
}

pub struct Slide<'a, Msg> {
    content: cosmic::Element<'a, Msg>,
    offset: Box<dyn Fn(Rectangle) -> Vector + 'a>,
    interactive: bool,
    _msg: PhantomData<Msg>,
}

impl<'a, Msg> Slide<'a, Msg> {
    /// When `false`, events are not forwarded to the content, so it neither
    /// reacts to input nor captures it.
    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }
}

impl<Msg> Widget<Msg, cosmic::Theme, cosmic::Renderer> for Slide<'_, Msg> {
    delegate::delegate! {
        to self.content.as_widget() {
            fn size(&self) -> Size<Length>;
            fn size_hint(&self) -> Size<Length>;
        }
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut [&mut self.content]);
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation<()>,
    ) {
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout.children().next().unwrap(),
            renderer,
            operation,
        );
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Msg>,
        viewport: &Rectangle,
    ) {
        if !self.interactive {
            return;
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout.children().next().unwrap(),
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let node = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let bounds = node.bounds();
        let offset = (self.offset)(bounds);
        layout::Node::with_children(
            bounds.size(),
            vec![node.move_to(Point::new(bounds.x + offset.x, bounds.y + offset.y))],
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout.children().next().unwrap(),
            cursor,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout.children().next().unwrap(),
            cursor,
            viewport,
            renderer,
        )
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Msg, cosmic::Theme, cosmic::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout.children().next().unwrap(),
            renderer,
            viewport,
            translation,
        )
    }

    fn drag_destinations(
        &self,
        state: &Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        dnd_rectangles: &mut cosmic::iced::core::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            state,
            layout.children().next().unwrap(),
            renderer,
            dnd_rectangles,
        );
    }
}

impl<'a, Msg: 'a> From<Slide<'a, Msg>> for cosmic::Element<'a, Msg> {
    fn from(widget: Slide<'a, Msg>) -> Self {
        cosmic::Element::new(widget)
    }
}
